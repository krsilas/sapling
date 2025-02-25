/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::PathBuf;
use std::str::FromStr;

use commit_cloud_service_lib::builder::SqlCommitCloudBuilder;
use commit_cloud_service_lib::checkout_locations::WorkspaceCheckoutLocation;
use commit_cloud_service_lib::heads::HeadExtraArgs;
use commit_cloud_service_lib::heads::WorkspaceHead;
use commit_cloud_service_lib::snapshots::SnapshotExtraArgs;
use commit_cloud_service_lib::snapshots::WorkspaceSnapshot;
use commit_cloud_service_lib::BasicOps;
use fbinit::FacebookInit;
use mercurial_types::HgChangesetId;
use mononoke_types::Timestamp;
use sql_construct::SqlConstruct;
#[fbinit::test]
async fn test_checkout_locations(_fb: FacebookInit) -> anyhow::Result<()> {
    let sql = SqlCommitCloudBuilder::with_sqlite_in_memory()?.new();
    let reponame = "test_repo".to_owned();
    let workspace = "user_testuser_default".to_owned();

    let args = WorkspaceCheckoutLocation {
        hostname: "testhost".to_owned(),
        commit: HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536").unwrap(),
        checkout_path: PathBuf::from("checkout/path"),
        shared_path: PathBuf::from("shared/path"),
        timestamp: Timestamp::now(),
        unixname: "testuser".to_owned(),
    };
    let expected = args.clone();

    assert!(
        sql.insert(reponame.clone(), workspace.clone(), args, ())
            .await?
    );

    let res: Vec<WorkspaceCheckoutLocation> = sql.get(reponame, workspace, ()).await?;
    assert!(res.len() == 1);

    assert!(expected.hostname == res[0].hostname);
    assert!(expected.commit == res[0].commit);
    assert!(expected.checkout_path == res[0].checkout_path);
    assert!(expected.shared_path == res[0].shared_path);
    assert!(expected.unixname == res[0].unixname);

    Ok(())
}

#[fbinit::test]
async fn test_snapshots(_fb: FacebookInit) -> anyhow::Result<()> {
    let sql = SqlCommitCloudBuilder::with_sqlite_in_memory()?.new();
    let reponame = "test_repo".to_owned();
    let workspace = "user_testuser_default".to_owned();

    let snapshot1 = WorkspaceSnapshot {
        commit: HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536").unwrap(),
    };

    let snapshot2 = WorkspaceSnapshot {
        commit: HgChangesetId::from_str("3e0e761030db6e479a7fb58b12881883f9f8c63f").unwrap(),
    };

    let removed_commits = vec![snapshot1.commit.clone()];

    assert!(
        sql.insert(reponame.clone(), workspace.clone(), snapshot1, None)
            .await?
    );
    assert!(
        sql.insert(reponame.clone(), workspace.clone(), snapshot2.clone(), None)
            .await?
    );

    let res: Vec<WorkspaceSnapshot> = sql.get(reponame.clone(), workspace.clone(), None).await?;
    assert!(res.len() == 2);

    assert!(
        BasicOps::<WorkspaceSnapshot>::delete(
            &sql,
            reponame.clone(),
            workspace.clone(),
            Some(SnapshotExtraArgs { removed_commits })
        )
        .await?
    );
    let res: Vec<WorkspaceSnapshot> = sql.get(reponame, workspace.clone(), None).await?;
    assert!(res.len() == 1);
    assert!(res[0].commit == snapshot2.commit);

    Ok(())
}

#[fbinit::test]
async fn test_heads(_fb: FacebookInit) -> anyhow::Result<()> {
    let sql = SqlCommitCloudBuilder::with_sqlite_in_memory()?.new();
    let reponame = "test_repo".to_owned();
    let workspace = "user_testuser_default".to_owned();

    let head1 = WorkspaceHead {
        commit: HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536").unwrap(),
    };

    let head2 = WorkspaceHead {
        commit: HgChangesetId::from_str("3e0e761030db6e479a7fb58b12881883f9f8c63f").unwrap(),
    };

    let removed_commits = vec![head1.commit.clone()];

    assert!(
        sql.insert(reponame.clone(), workspace.clone(), head1, None)
            .await?
    );
    assert!(
        sql.insert(reponame.clone(), workspace.clone(), head2.clone(), None)
            .await?
    );

    let res: Vec<WorkspaceHead> = sql.get(reponame.clone(), workspace.clone(), None).await?;
    assert!(res.len() == 2);

    assert!(
        BasicOps::<WorkspaceHead>::delete(
            &sql,
            reponame.clone(),
            workspace.clone(),
            Some(HeadExtraArgs { removed_commits })
        )
        .await?
    );
    let res: Vec<WorkspaceHead> = sql.get(reponame, workspace.clone(), None).await?;
    assert!(res.len() == 1);
    assert!(res[0].commit == head2.commit);

    Ok(())
}
