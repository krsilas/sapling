/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::PathBuf;

use ::sql_ext::mononoke_queries;
use async_trait::async_trait;
use mercurial_types::HgChangesetId;
use mononoke_types::Timestamp;
use sql::Connection;

use crate::BasicOps;
use crate::SqlCommitCloud;

#[derive(Clone)]
pub struct WorkspaceCheckoutLocation {
    pub hostname: String,
    pub commit: HgChangesetId,
    pub checkout_path: PathBuf,
    pub shared_path: PathBuf,
    pub timestamp: Timestamp,
    pub unixname: String,
}

mononoke_queries! {
    pub(crate) read GetCheckoutLocations(reponame: String, workspace: String) -> (String, String, String, HgChangesetId, Timestamp, String) {
        "SELECT
            `hostname`,
            `checkout_path`,
            `shared_path`,
            `commit` ,
            `timestamp`,
            `unixname`
        FROM `checkoutlocations`
        WHERE `reponame`={reponame} AND `workspace`={workspace}"
    }

    pub(crate) write InsertCheckoutLocations(reponame: String, workspace: String, hostname: String, commit: HgChangesetId, checkout_path: String, shared_path: String, unixname: String, timestamp: Timestamp) {
        none,
        mysql("INSERT INTO `checkoutlocations` (
            `reponame`,
            `workspace`,
            `hostname`,
            `commit`,
            `checkout_path`,
            `shared_path` ,
            `unixname`,
            `timestamp`
        ) VALUES (
            {reponame},
            {workspace},
            {hostname},
            {commit},
            {checkout_path},
            {shared_path},
            {unixname},
            {timestamp})
        ON DUPLICATE KEY UPDATE
            `commit` = {commit},
            `timestamp` = current_timestamp")

        sqlite("INSERT OR REPLACE INTO `checkoutlocations` (
            `reponame`,
            `workspace`,
            `hostname`,
            `commit`,
            `checkout_path`,
            `shared_path`,
            `unixname`,
            `timestamp`
        ) VALUES (
            {reponame},
            {workspace},
            {hostname},
            {commit},
            {checkout_path},
            {shared_path},
            {unixname},
            {timestamp})")
    }

}

#[async_trait]
impl BasicOps<WorkspaceCheckoutLocation> for SqlCommitCloud {
    type ExtraArgs = ();

    async fn get(
        &self,
        reponame: String,
        workspace: String,
        _extra_arg: (),
    ) -> anyhow::Result<Vec<WorkspaceCheckoutLocation>> {
        let rows =
            GetCheckoutLocations::query(&self.connections.read_connection, &reponame, &workspace)
                .await?;

        rows.into_iter()
            .map(
                |(hostname, checkout_path, shared_path, commit, timestamp, unixname)| {
                    Ok(WorkspaceCheckoutLocation {
                        hostname,
                        commit,
                        checkout_path: PathBuf::from(checkout_path),
                        shared_path: PathBuf::from(shared_path),
                        timestamp,
                        unixname,
                    })
                },
            )
            .collect::<anyhow::Result<Vec<WorkspaceCheckoutLocation>>>()
    }

    async fn insert(
        &self,
        reponame: String,
        workspace: String,
        data: WorkspaceCheckoutLocation,
        _extra_arg: (),
    ) -> anyhow::Result<bool> {
        InsertCheckoutLocations::query(
            &self.connections.write_connection,
            &reponame,
            &workspace,
            &data.hostname,
            &data.commit,
            &data.checkout_path.display().to_string(),
            &data.shared_path.display().to_string(),
            &data.unixname,
            &data.timestamp,
        )
        .await
        .map(|res| res.affected_rows() > 0)
    }
    async fn delete(
        &self,
        _reponame: String,
        _workspace: String,
        _extra_arg: (),
    ) -> anyhow::Result<bool> {
        // Checkout locations delete op is never used
        unimplemented!("delete is not implemented for checkout locations")
    }
    async fn update(
        &self,
        _reponame: String,
        _workspace: String,
        _extra_arg: (),
    ) -> anyhow::Result<bool> {
        // Checkout locations update op endpoint is never used
        unimplemented!("delete is not implemented for checkout locations")
    }
}
