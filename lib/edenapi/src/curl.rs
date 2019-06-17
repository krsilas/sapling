// Copyright Facebook, Inc. 2019

use std::{
    cmp,
    sync::mpsc::channel,
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::Instant,
};

use bytes::Bytes;
use curl::{
    self,
    easy::{Easy2, Handler, HttpVersion, List},
    multi::Multi,
};
use failure::{bail, ensure, err_msg, Fallible};
use itertools::Itertools;
use log;
use serde::{de::DeserializeOwned, Serialize};
use serde_cbor::Deserializer;
use url::Url;

use driver::MultiDriver;
use handler::Collector;
use revisionstore::{Delta, Metadata, MutableDeltaStore, MutableHistoryStore};
use types::{
    api::{DataRequest, DataResponse, HistoryRequest, HistoryResponse, TreeRequest},
    DataEntry, HistoryEntry, Key, Node, RepoPathBuf, WireHistoryEntry,
};

use crate::api::EdenApi;
use crate::config::{ClientCreds, Config};
use crate::progress::{ProgressFn, ProgressManager};
use crate::stats::DownloadStats;

mod driver;
mod handler;

mod paths {
    pub const HEALTH_CHECK: &str = "/health_check";
    pub const HOSTNAME: &str = "/hostname";
    pub const DATA: &str = "eden/data";
    pub const HISTORY: &str = "eden/history";
    pub const TREES: &str = "eden/trees";
    pub const PREFETCH_TREES: &str = "eden/trees/prefetch";
}

/// A thread-safe wrapper around a `curl::Multi` handle.
///
/// Ordinarily, a `curl::Multi` handle does not implement `Send` and `Sync`
/// because it contains a mutable pointer to the underlying C curl handle.
/// However, accoding to curl's documentation [1]:
///
/// > You must never share the same handle in multiple threads. You can pass the
/// > handles around among threads, but you must never use a single handle from
/// > more than one thread at any given time.
///
/// This means that as long as we wrap the handle in a Mutex to prevent concurrent
/// access from multiple threads, the handle can safely be shared across threads.
/// Note that given that the underlying `curl::Multi` handle is created in `new()`
/// and is private to the struct, the pointer contained therein can be assumed to
/// be unique, avoiding issues with mutable pointer aliasing.
///
/// [1]: https://curl.haxx.se/libcurl/c/threadsafe.html
#[derive(Clone)]
struct SyncMulti(Arc<Mutex<Multi>>);

impl SyncMulti {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Multi::new())))
    }

    fn lock(&self) -> MutexGuard<'_, Multi> {
        self.0.lock().expect("curl multi handle lock poisoned")
    }
}

unsafe impl Send for SyncMulti {}
unsafe impl Sync for SyncMulti {}

pub struct EdenApiCurlClient {
    multi: SyncMulti,
    base_url: Url,
    repo: String,
    creds: Option<ClientCreds>,
    data_batch_size: Option<usize>,
    history_batch_size: Option<usize>,
    validate: bool,
    stream_data: bool,
    stream_history: bool,
    stream_trees: bool,
}

// Public API.
impl EdenApiCurlClient {
    pub fn new(config: Config) -> Fallible<Self> {
        let base_url = match config.base_url {
            Some(url) => url,
            None => bail!("No base URL specified"),
        };

        let repo = match config.repo {
            Some(repo) => repo,
            None => bail!("No repo name specified"),
        };

        Ok(Self {
            multi: SyncMulti::new(),
            base_url,
            repo,
            creds: config.creds,
            data_batch_size: config.data_batch_size,
            history_batch_size: config.history_batch_size,
            validate: config.validate,
            stream_data: config.stream_data,
            stream_history: config.stream_history,
            stream_trees: config.stream_trees,
        })
    }
}

impl EdenApi for EdenApiCurlClient {
    fn health_check(&self) -> Fallible<()> {
        let handler = Collector::new();
        let mut handle = new_easy_handle(self.creds.as_ref(), handler)?;
        let url = self.base_url.join(paths::HEALTH_CHECK)?;
        handle.url(url.as_str())?;
        handle.get(true)?;
        handle.perform()?;

        let code = handle.response_code()?;
        ensure!(code == 200, "Received HTTP status code {}", code);

        let response = String::from_utf8_lossy(&handle.get_ref().data());
        ensure!(
            response == "I_AM_ALIVE",
            "Unexpected response: {:?}",
            &response
        );

        Ok(())
    }

    fn hostname(&self) -> Fallible<String> {
        let handler = Collector::new();
        let mut handle = new_easy_handle(self.creds.as_ref(), handler)?;
        let url = self.base_url.join(paths::HOSTNAME)?;
        handle.url(url.as_str())?;
        handle.get(true)?;
        handle.perform()?;

        let code = handle.response_code()?;
        ensure!(code == 200, "Received HTTP status code {}", code);

        let response = String::from_utf8(handle.get_ref().data().to_vec())?;
        Ok(response)
    }

    fn get_files(
        &self,
        keys: Vec<Key>,
        store: &mut MutableDeltaStore,
        progress: Option<ProgressFn>,
    ) -> Fallible<DownloadStats> {
        self.get_data(paths::DATA, keys, store, progress)
    }

    fn get_history(
        &self,
        keys: Vec<Key>,
        store: &mut MutableHistoryStore,
        max_depth: Option<u32>,
        progress: Option<ProgressFn>,
    ) -> Fallible<DownloadStats> {
        log::debug!("Fetching {} files", keys.len());

        let mut url = self.repo_base_url()?.join(paths::HISTORY)?;
        if self.stream_history {
            url.set_query(Some("stream=true"));
        }

        let batch_size = self.history_batch_size.unwrap_or(cmp::max(keys.len(), 1));
        let num_requests = (keys.len() + batch_size - 1) / batch_size;

        log::debug!("Using batch size: {}", batch_size);
        log::debug!("Preparing {} requests", num_requests);

        let chunks = keys.into_iter().chunks(batch_size);
        let requests = (&chunks).into_iter().map(|batch| HistoryRequest {
            keys: batch.into_iter().collect(),
            depth: max_depth,
        });

        let mut multi = self.multi.lock();

        let mut num_responses = 0;
        let mut num_entries = 0;
        let stats = if self.stream_history {
            multi_request(
                &mut multi,
                &url,
                self.creds.as_ref(),
                requests,
                progress,
                |response: Vec<(RepoPathBuf, WireHistoryEntry)>| {
                    num_responses += 1;
                    for (path, entry) in response {
                        num_entries += 1;
                        let entry = HistoryEntry::from_wire(entry, path);
                        store.add_entry(&entry)?;
                    }
                    Ok(())
                },
            )?
        } else {
            multi_request(
                &mut multi,
                &url,
                self.creds.as_ref(),
                requests,
                progress,
                |response: Vec<HistoryResponse>| {
                    num_responses += 1;
                    for entry in response.into_iter().flatten() {
                        num_entries += 1;
                        store.add_entry(&entry)?;
                    }
                    Ok(())
                },
            )?
        };

        log::debug!(
            "Received {} responses with {} total entries",
            num_responses,
            num_entries
        );
        Ok(stats)
    }

    fn get_trees(
        &self,
        keys: Vec<Key>,
        store: &mut MutableDeltaStore,
        progress: Option<ProgressFn>,
    ) -> Fallible<DownloadStats> {
        self.get_data(paths::TREES, keys, store, progress)
    }

    fn prefetch_trees(
        &self,
        rootdir: RepoPathBuf,
        mfnodes: Vec<Node>,
        basemfnodes: Vec<Node>,
        depth: Option<usize>,
        store: &mut MutableDeltaStore,
        progress: Option<ProgressFn>,
    ) -> Fallible<DownloadStats> {
        let mut url = self.repo_base_url()?.join(paths::PREFETCH_TREES)?;
        if self.stream_trees {
            url.set_query(Some("stream=true"));
        }

        let creds = self.creds.as_ref();
        let requests = vec![TreeRequest::new(rootdir, mfnodes, basemfnodes, depth)];

        if self.stream_trees {
            multi_request_threaded(
                self.multi.clone(),
                &url,
                creds,
                requests,
                progress,
                |entries| {
                    let response = DataResponse::new(entries);
                    add_data_response(store, response, self.validate)
                },
            )
        } else {
            multi_request_threaded(
                self.multi.clone(),
                &url,
                creds,
                requests,
                progress,
                |responses| {
                    for response in responses {
                        add_data_response(store, response, self.validate)?;
                    }
                    Ok(())
                },
            )
        }
    }
}

// Private methods.
impl EdenApiCurlClient {
    fn repo_base_url(&self) -> Fallible<Url> {
        Ok(self.base_url.join(&format!("{}/", &self.repo))?)
    }

    fn get_data(
        &self,
        path: &str,
        keys: Vec<Key>,
        store: &mut MutableDeltaStore,
        progress: Option<ProgressFn>,
    ) -> Fallible<DownloadStats> {
        log::debug!("Fetching data for {} keys", keys.len());

        let mut url = self.repo_base_url()?.join(path)?;
        if self.stream_data {
            url.set_query(Some("stream=true"));
        }

        let batch_size = self.data_batch_size.unwrap_or(cmp::max(keys.len(), 1));
        let num_requests = (keys.len() + batch_size - 1) / batch_size;

        log::debug!("Using batch size: {}", batch_size);
        log::debug!("Preparing {} requests", num_requests);

        let mut requests = Vec::with_capacity(num_requests);
        for batch in &keys.into_iter().chunks(batch_size) {
            let keys = batch.into_iter().collect();
            requests.push(DataRequest { keys });
        }

        let mut num_responses = 0;
        let mut num_entries = 0;
        let stats = if self.stream_data {
            multi_request_threaded(
                self.multi.clone(),
                &url,
                self.creds.as_ref(),
                requests,
                progress,
                |entries: Vec<DataEntry>| {
                    num_responses += 1;
                    num_entries += entries.len();
                    let response = DataResponse::new(entries);
                    add_data_response(store, response, self.validate)
                },
            )?
        } else {
            multi_request_threaded(
                self.multi.clone(),
                &url,
                self.creds.as_ref(),
                requests,
                progress,
                |responses: Vec<DataResponse>| {
                    for response in responses {
                        num_responses += 1;
                        num_entries += response.entries.len();
                        add_data_response(store, response, self.validate)?;
                    }
                    Ok(())
                },
            )?
        };

        log::debug!(
            "Received {} responses with {} total entries",
            num_responses,
            num_entries
        );
        Ok(stats)
    }
}

/// Send multiple concurrent POST requests using the given requests as the
/// CBOR payload of each respective request. Assumes that the responses are
/// CBOR encoded, and automatically deserializes them before passing
/// them to the given callback.
fn multi_request<'a, R, I, T, F>(
    multi: &'a mut Multi,
    url: &Url,
    creds: Option<&ClientCreds>,
    requests: I,
    progress_cb: Option<ProgressFn>,
    mut response_cb: F,
) -> Fallible<DownloadStats>
where
    R: Serialize,
    I: IntoIterator<Item = R>,
    T: DeserializeOwned,
    F: FnMut(Vec<T>) -> Fallible<()>,
{
    let requests = requests.into_iter().collect::<Vec<_>>();
    let num_requests = requests.len();

    let mut progress = ProgressManager::with_capacity(num_requests);
    let mut driver = MultiDriver::with_capacity(multi, num_requests);
    driver.fail_early(true);

    for request in requests {
        let handle = progress.register();
        let handler = Collector::with_progress(handle);
        let mut easy = new_easy_handle(creds, handler)?;
        prepare_cbor_post(&mut easy, &url, &request)?;
        driver.add(easy)?;
    }

    progress.set_callback(progress_cb);
    driver.set_progress_manager(progress);

    log::debug!("Performing {} requests", num_requests);
    let start = Instant::now();

    driver.perform(|res| {
        let mut easy = res?;
        let code = easy.response_code()?;
        let data = easy.get_ref().data();

        if code >= 400 {
            let msg = String::from_utf8_lossy(data);
            bail!(
                "Received HTTP status code {} with response: {:?}",
                code,
                msg
            );
        }

        let response = Deserializer::from_slice(data)
            .into_iter()
            .collect::<Result<Vec<T>, serde_cbor::error::Error>>()?;
        response_cb(response)
    })?;

    let elapsed = start.elapsed();
    let progress = driver.progress().unwrap();
    let progstats = progress.stats();
    let latency = progress
        .first_response_time()
        .unwrap_or(start)
        .duration_since(start);

    let dlstats = DownloadStats {
        downloaded: progstats.downloaded,
        uploaded: progstats.uploaded,
        requests: num_requests,
        time: elapsed,
        latency,
    };

    log::info!("{}", &dlstats);

    Ok(dlstats)
}

/// Same as `multi_request`, except the HTTP transfers will be handled by
/// separate thread, while the user-provided response callback will be
/// run on the main thread. This allows the callback to perform potentially
/// expensive and/or blocking operations upon receiving a response
/// without affecting the other ongoing HTTP transfers.
fn multi_request_threaded<R, I, T, F>(
    multi: SyncMulti,
    url: &Url,
    creds: Option<&ClientCreds>,
    requests: I,
    progress_cb: Option<ProgressFn>,
    mut response_cb: F,
) -> Fallible<DownloadStats>
where
    R: Serialize + Send + 'static,
    I: IntoIterator<Item = R>,
    T: DeserializeOwned + Send + Sync + 'static,
    F: FnMut(Vec<T>) -> Fallible<()>,
{
    // Convert arguments to owned types since these will be sent
    // to a new thread, which requires captured values to have a
    // 'static lifetime.
    let url = url.clone();
    let creds = creds.cloned();
    let requests = requests.into_iter().collect::<Vec<_>>();

    log::debug!("Spawning HTTP I/O thread");
    let (tx, rx) = channel();
    let iothread = thread::spawn(move || {
        let mut multi = multi.lock();
        multi_request(
            &mut multi,
            &url,
            creds.as_ref(),
            requests,
            progress_cb,
            |response: Vec<T>| Ok(tx.send(response)?),
        )
    });

    for response in rx {
        response_cb(response)?;
    }

    iothread
        .join()
        .map_err(|_| err_msg("I/O thread panicked"))?
}

/// Configure a new curl::Easy2 handle with appropriate default settings.
fn new_easy_handle<H: Handler>(creds: Option<&ClientCreds>, handler: H) -> Fallible<Easy2<H>> {
    let mut handle = Easy2::new(handler);
    if let Some(ClientCreds { ref certs, ref key }) = creds {
        handle.ssl_cert(certs)?;
        handle.ssl_key(key)?;
    }
    handle.http_version(HttpVersion::V2)?;
    handle.progress(true)?;
    Ok(handle)
}

/// Configure the given Easy2 handle to perform a POST request.
/// The given payload will be serialized as CBOR and used as the request body.
fn prepare_cbor_post<H, R: Serialize>(easy: &mut Easy2<H>, url: &Url, request: &R) -> Fallible<()> {
    let payload = serde_cbor::to_vec(&request)?;

    easy.url(url.as_str())?;
    easy.post(true)?;
    easy.post_fields_copy(&payload)?;

    let mut headers = List::new();
    headers.append("Content-Type: application/cbor")?;
    easy.http_headers(headers)?;

    Ok(())
}

fn add_delta(store: &mut MutableDeltaStore, key: Key, data: Bytes) -> Fallible<()> {
    let metadata = Metadata {
        size: Some(data.len() as u64),
        flags: None,
    };
    let delta = Delta {
        data,
        base: None,
        key,
    };
    store.add(&delta, &metadata)?;
    Ok(())
}

fn add_data_response(
    store: &mut MutableDeltaStore,
    response: DataResponse,
    validate: bool,
) -> Fallible<()> {
    for entry in response {
        let data = entry.data(validate)?;
        add_delta(store, entry.key().clone(), data)?;
    }
    Ok(())
}
