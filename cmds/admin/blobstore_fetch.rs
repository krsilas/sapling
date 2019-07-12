// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::fmt;

use clap::ArgMatches;
use failure_ext::{format_err, Error, Result};
use futures::prelude::*;
use futures_ext::{try_boxfuture, BoxFuture, FutureExt};
use std::sync::Arc;

use blobstore::Blobstore;
use blobstore_factory::{make_blobstore, SqliteFactory, XdbFactory};
use cacheblob::{new_memcache_blobstore, CacheBlobstoreExt};
use censoredblob::{CensoredBlob, SqlCensoredContentStore};
use cloned::cloned;
use cmdlib::args;
use context::CoreContext;
use futures::future;
use mercurial_types::{HgChangesetEnvelope, HgFileEnvelope, HgManifestEnvelope};
use metaconfig_types::{BlobConfig, BlobstoreId, Censoring, MetadataDBConfig, StorageConfig};
use mononoke_types::{BlobstoreBytes, BlobstoreValue, FileContents, RepositoryId};
use prefixblob::PrefixBlobstore;
use scuba_ext::{ScubaSampleBuilder, ScubaSampleBuilderExt};
use slog::{info, warn, Logger};
use std::collections::HashMap;
use std::iter::FromIterator;

fn get_blobconfig(blob_config: BlobConfig, inner_blobstore_id: Option<u64>) -> Result<BlobConfig> {
    match inner_blobstore_id {
        None => Ok(blob_config),
        Some(inner_blobstore_id) => match blob_config {
            BlobConfig::Multiplexed { blobstores, .. } => {
                let seeked_id = BlobstoreId::new(inner_blobstore_id);
                blobstores
                    .into_iter()
                    .find_map(|(blobstore_id, blobstore)| {
                        if blobstore_id == seeked_id {
                            Some(blobstore)
                        } else {
                            None
                        }
                    })
                    .ok_or(format_err!(
                        "could not find a blobstore with id {}",
                        inner_blobstore_id
                    ))
            }
            _ => Err(format_err!(
                "inner-blobstore-id supplied but blobstore is not multiplexed"
            )),
        },
    }
}

fn get_blobstore(
    repo_id: RepositoryId,
    storage_config: StorageConfig,
    inner_blobstore_id: Option<u64>,
) -> BoxFuture<Arc<dyn Blobstore>, Error> {
    let blobconfig = try_boxfuture!(get_blobconfig(storage_config.blobstore, inner_blobstore_id));

    match storage_config.dbconfig {
        MetadataDBConfig::LocalDB { path } => {
            make_blobstore(repo_id, &blobconfig, &SqliteFactory::new(path), None)
        }
        MetadataDBConfig::Mysql {
            db_address,
            sharded_filenodes,
        } => make_blobstore(
            repo_id,
            &blobconfig,
            &XdbFactory::new(db_address, None, sharded_filenodes),
            None,
        ),
    }
}

pub fn subcommand_blobstore_fetch(
    logger: Logger,
    matches: &ArgMatches<'_>,
    sub_m: &ArgMatches<'_>,
) -> BoxFuture<(), Error> {
    let repo_id = try_boxfuture!(args::get_repo_id(&matches));
    let (_, config) = try_boxfuture!(args::get_config(&matches));
    let censoring = config.censoring;
    let storage_config = config.storage_config;
    let inner_blobstore_id = args::get_u64_opt(&sub_m, "inner-blobstore-id");
    let blobstore_fut = get_blobstore(repo_id, storage_config, inner_blobstore_id);

    let common_config = try_boxfuture!(args::read_common_config(&matches));
    let scuba_censored_table = common_config.scuba_censored_table;
    let scuba_censorship_builder = ScubaSampleBuilder::with_opt_table(scuba_censored_table);

    let ctx = CoreContext::test_mock();
    let key = sub_m.value_of("KEY").unwrap().to_string();
    let decode_as = sub_m.value_of("decode-as").map(|val| val.to_string());
    let use_memcache = sub_m.value_of("use-memcache").map(|val| val.to_string());
    let no_prefix = sub_m.is_present("no-prefix");

    let maybe_censored_blobs_fut = match censoring {
        Censoring::Enabled => {
            let censored_blobs_store: Arc<_> = Arc::new(
                args::open_sql::<SqlCensoredContentStore>(&matches)
                    .expect("Failed to open the db with censored_blobs_store"),
            );

            censored_blobs_store
                .get_all_censored_blobs()
                .map_err(Error::from)
                .map(HashMap::from_iter)
                .map(Some)
                .left_future()
        }
        Censoring::Disabled => future::ok(None).right_future(),
    };

    let value_fut = blobstore_fut.join(maybe_censored_blobs_fut).and_then({
        cloned!(logger, key, ctx);
        move |(blobstore, maybe_censored_blobs)| {
            info!(logger, "using blobstore: {:?}", blobstore);
            get_from_sources(
                use_memcache,
                blobstore,
                no_prefix,
                key.clone(),
                ctx,
                maybe_censored_blobs,
                scuba_censorship_builder,
                repo_id,
            )
        }
    });

    value_fut
        .map({
            cloned!(key);
            move |value| {
                println!("{:?}", value);
                if let Some(value) = value {
                    let decode_as = decode_as.as_ref().and_then(|val| {
                        let val = val.as_str();
                        if val == "auto" {
                            detect_decode(&key, &logger)
                        } else {
                            Some(val)
                        }
                    });

                    match decode_as {
                        Some("changeset") => display(&HgChangesetEnvelope::from_blob(value.into())),
                        Some("manifest") => display(&HgManifestEnvelope::from_blob(value.into())),
                        Some("file") => display(&HgFileEnvelope::from_blob(value.into())),
                        // TODO: (rain1) T30974137 add a better way to print out file contents
                        Some("contents") => println!("{:?}", FileContents::from_blob(value.into())),
                        _ => (),
                    }
                }
            }
        })
        .boxify()
}

fn get_from_sources<T: Blobstore + Clone>(
    use_memcache: Option<String>,
    blobstore: T,
    no_prefix: bool,
    key: String,
    ctx: CoreContext,
    censored_blobs: Option<HashMap<String, String>>,
    scuba_censorship_builder: ScubaSampleBuilder,
    repo_id: RepositoryId,
) -> BoxFuture<Option<BlobstoreBytes>, Error> {
    let empty_prefix = "".to_string();

    match use_memcache {
        Some(mode) => {
            let blobstore = new_memcache_blobstore(blobstore, "multiplexed", "").unwrap();
            let blobstore = match no_prefix {
                false => PrefixBlobstore::new(blobstore, repo_id.prefix()),
                true => PrefixBlobstore::new(blobstore, empty_prefix),
            };
            let blobstore = CensoredBlob::new(blobstore, censored_blobs, scuba_censorship_builder);
            get_cache(ctx.clone(), &blobstore, key.clone(), mode)
        }
        None => {
            let blobstore = match no_prefix {
                false => PrefixBlobstore::new(blobstore, repo_id.prefix()),
                true => PrefixBlobstore::new(blobstore, empty_prefix),
            };
            let blobstore = CensoredBlob::new(blobstore, censored_blobs, scuba_censorship_builder);
            blobstore.get(ctx, key.clone()).boxify()
        }
    }
}

fn display<T>(res: &Result<T>)
where
    T: fmt::Display + fmt::Debug,
{
    match res {
        Ok(val) => println!("---\n{}---", val),
        err => println!("{:?}", err),
    }
}

fn detect_decode(key: &str, logger: &Logger) -> Option<&'static str> {
    // Use a simple heuristic to figure out how to decode this key.
    if key.find("hgchangeset.").is_some() {
        info!(logger, "Detected changeset key");
        Some("changeset")
    } else if key.find("hgmanifest.").is_some() {
        info!(logger, "Detected manifest key");
        Some("manifest")
    } else if key.find("hgfilenode.").is_some() {
        info!(logger, "Detected file key");
        Some("file")
    } else if key.find("content.").is_some() {
        info!(logger, "Detected content key");
        Some("contents")
    } else {
        warn!(
            logger,
            "Unable to detect how to decode this blob based on key";
            "key" => key,
        );
        None
    }
}

fn get_cache<B: CacheBlobstoreExt>(
    ctx: CoreContext,
    blobstore: &B,
    key: String,
    mode: String,
) -> BoxFuture<Option<BlobstoreBytes>, Error> {
    if mode == "cache-only" {
        blobstore.get_cache_only(ctx, key)
    } else if mode == "no-fill" {
        blobstore.get_no_cache_fill(ctx, key)
    } else {
        blobstore.get(ctx, key)
    }
}
