/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::{format_err, Error};
use context::CoreContext;
use fbinit::FacebookInit;
use filenodes::FilenodeInfo;
use futures_preview::compat::Future01CompatExt;
use mercurial_types::HgFileNodeId;
use mercurial_types_mocks::nodehash::{
    ONES_CSID, ONES_FNID, THREES_CSID, THREES_FNID, TWOS_CSID, TWOS_FNID,
};
use mononoke_types::{MPath, RepoPath, RepositoryId};
use mononoke_types_mocks::repo::{REPO_ONE, REPO_ZERO};
use sql::queries;
use sql::{rusqlite::Connection as SqliteConnection, Connection};
use sql_ext::SqlConstructors;
use tokio_preview as tokio;

use crate::builder::{NewFilenodesBuilder, SQLITE_INSERT_CHUNK_SIZE};
use crate::local_cache::{test::HashMapCache, LocalCache};
use crate::reader::FilenodesReader;
use crate::writer::FilenodesWriter;

fn build_shard() -> Result<Connection, Error> {
    let con = SqliteConnection::open_in_memory()?;
    con.execute_batch(NewFilenodesBuilder::get_up_query())?;
    Ok(Connection::with_sqlite(con))
}

fn build_reader_writer(shards: Vec<Connection>) -> (FilenodesReader, FilenodesWriter) {
    let reader = FilenodesReader::new(shards.clone(), shards.clone());
    let writer = FilenodesWriter::new(SQLITE_INSERT_CHUNK_SIZE, shards.clone(), shards.clone());
    (reader, writer)
}

async fn check_roundtrip(
    ctx: &CoreContext,
    repo_id: RepositoryId,
    reader: &FilenodesReader,
    writer: &FilenodesWriter,
    info: FilenodeInfo,
) -> Result<(), Error> {
    assert_eq!(
        reader
            .get_filenode(&ctx, repo_id, info.path.clone(), info.filenode)
            .await?,
        None
    );

    writer
        .insert_filenodes(&ctx, repo_id, vec![info.clone()], false)
        .await?;

    assert_eq!(
        reader
            .get_filenode(&ctx, repo_id, info.path.clone(), info.filenode)
            .await?,
        Some(info),
    );

    Ok(())
}

#[fbinit::test]
async fn test_basic(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);

    let shard = build_shard()?;
    let (reader, writer) = build_reader_writer(vec![shard]);

    let info = FilenodeInfo {
        path: RepoPath::FilePath(MPath::new(b"test")?),
        filenode: ONES_FNID,
        p1: Some(TWOS_FNID),
        p2: None,
        copyfrom: None,
        linknode: ONES_CSID,
    };

    check_roundtrip(&ctx, REPO_ZERO, &reader, &writer, info).await?;

    Ok(())
}

#[fbinit::test]
async fn read_copy_info(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);

    let shard = build_shard()?;
    let (reader, writer) = build_reader_writer(vec![shard]);

    let from = FilenodeInfo {
        path: RepoPath::FilePath(MPath::new(b"from")?),
        filenode: ONES_FNID,
        p1: None,
        p2: None,
        copyfrom: None,
        linknode: ONES_CSID,
    };

    writer
        .insert_filenodes(&ctx, REPO_ZERO, vec![from.clone()], false)
        .await?;

    let info = FilenodeInfo {
        path: RepoPath::FilePath(MPath::new(b"test")?),
        filenode: TWOS_FNID,
        p1: None,
        p2: None,
        copyfrom: Some((from.path.clone(), from.filenode)),
        linknode: TWOS_CSID,
    };

    check_roundtrip(&ctx, REPO_ZERO, &reader, &writer, info).await?;

    Ok(())
}

#[fbinit::test]
async fn test_repo_ids(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);

    let shard = build_shard()?;
    let (reader, writer) = build_reader_writer(vec![shard]);

    let info = root_first_filenode();

    writer
        .insert_filenodes(&ctx, REPO_ZERO, vec![info.clone()], false)
        .await?;

    assert_filenode(
        &ctx,
        &reader,
        &info.path,
        info.filenode,
        REPO_ZERO,
        info.clone(),
    )
    .await?;
    assert_no_filenode(&ctx, &reader, &info.path, info.filenode, REPO_ONE).await?;

    Ok(())
}

queries! {
    write DeleteCopyInfo() {
        none,
        "DELETE FROM fixedcopyinfo"
    }
}

#[fbinit::test]
async fn test_fallback_on_missing_copy_info(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);

    let master = build_shard()?;
    let replica = build_shard()?;

    // Populate both master and replica with the same filenodes (to simulate replication).
    FilenodesWriter::new(
        SQLITE_INSERT_CHUNK_SIZE,
        vec![master.clone()],
        vec![master.clone()],
    )
    .insert_filenodes(
        &ctx,
        REPO_ZERO,
        vec![copied_from_filenode(), copied_filenode()],
        false,
    )
    .await?;

    FilenodesWriter::new(
        SQLITE_INSERT_CHUNK_SIZE,
        vec![replica.clone()],
        vec![replica.clone()],
    )
    .insert_filenodes(
        &ctx,
        REPO_ZERO,
        vec![copied_from_filenode(), copied_filenode()],
        false,
    )
    .await?;

    // Now, delete the copy info from the replica.
    DeleteCopyInfo::query(&replica).compat().await?;

    let reader = FilenodesReader::new(vec![replica], vec![master]);
    let info = copied_filenode();
    assert_filenode(
        &ctx,
        &reader,
        &info.path,
        info.filenode,
        REPO_ZERO,
        info.clone(),
    )
    .await?;

    Ok(())
}

fn root_first_filenode() -> FilenodeInfo {
    FilenodeInfo {
        path: RepoPath::root(),
        filenode: ONES_FNID,
        p1: None,
        p2: None,
        copyfrom: None,
        linknode: ONES_CSID,
    }
}

fn root_second_filenode() -> FilenodeInfo {
    FilenodeInfo {
        path: RepoPath::root(),
        filenode: TWOS_FNID,
        p1: Some(ONES_FNID),
        p2: None,
        copyfrom: None,
        linknode: TWOS_CSID,
    }
}

fn root_merge_filenode() -> FilenodeInfo {
    FilenodeInfo {
        path: RepoPath::root(),
        filenode: THREES_FNID,
        p1: Some(ONES_FNID),
        p2: Some(TWOS_FNID),
        copyfrom: None,
        linknode: THREES_CSID,
    }
}

fn file_a_first_filenode() -> FilenodeInfo {
    FilenodeInfo {
        path: RepoPath::file("a").unwrap(),
        filenode: ONES_FNID,
        p1: None,
        p2: None,
        copyfrom: None,
        linknode: ONES_CSID,
    }
}

fn file_b_first_filenode() -> FilenodeInfo {
    FilenodeInfo {
        path: RepoPath::file("b").unwrap(),
        filenode: TWOS_FNID,
        p1: None,
        p2: None,
        copyfrom: None,
        linknode: TWOS_CSID,
    }
}

fn copied_from_filenode() -> FilenodeInfo {
    FilenodeInfo {
        path: RepoPath::file("copiedfrom").unwrap(),
        filenode: ONES_FNID,
        p1: None,
        p2: None,
        copyfrom: None,
        linknode: TWOS_CSID,
    }
}

fn copied_filenode() -> FilenodeInfo {
    FilenodeInfo {
        path: RepoPath::file("copiedto").unwrap(),
        filenode: TWOS_FNID,
        p1: None,
        p2: None,
        copyfrom: Some((RepoPath::file("copiedfrom").unwrap(), ONES_FNID)),
        linknode: TWOS_CSID,
    }
}

async fn do_add_filenodes(
    ctx: &CoreContext,
    writer: &FilenodesWriter,
    to_insert: Vec<FilenodeInfo>,
    repo_id: RepositoryId,
) -> Result<(), Error> {
    writer
        .insert_filenodes(&ctx, repo_id, to_insert, false)
        .await?;
    Ok(())
}

async fn do_add_filenode(
    ctx: &CoreContext,
    writer: &FilenodesWriter,
    node: FilenodeInfo,
    repo_id: RepositoryId,
) -> Result<(), Error> {
    do_add_filenodes(ctx, writer, vec![node], repo_id).await?;
    Ok(())
}

async fn assert_no_filenode(
    ctx: &CoreContext,
    reader: &FilenodesReader,
    path: &RepoPath,
    hash: HgFileNodeId,
    repo_id: RepositoryId,
) -> Result<(), Error> {
    let res = reader
        .get_filenode(&ctx, repo_id, path.clone(), hash)
        .await?;
    assert!(res.is_none());
    Ok(())
}

async fn assert_filenode(
    ctx: &CoreContext,
    reader: &FilenodesReader,
    path: &RepoPath,
    hash: HgFileNodeId,
    repo_id: RepositoryId,
    expected: FilenodeInfo,
) -> Result<(), Error> {
    let res = reader
        .get_filenode(&ctx, repo_id, path.clone(), hash)
        .await?
        .ok_or(format_err!("not found: {}", hash))?;
    assert_eq!(res, expected);
    Ok(())
}

async fn assert_all_filenodes(
    ctx: &CoreContext,
    reader: &FilenodesReader,
    path: &RepoPath,
    repo_id: RepositoryId,
    expected: &Vec<FilenodeInfo>,
) -> Result<(), Error> {
    let res = reader
        .get_all_filenodes_for_path(&ctx, repo_id, path.clone())
        .await?;
    assert_eq!(&res, expected);
    Ok(())
}

macro_rules! filenodes_tests {
    ($test_suite_name:ident, $create_db:ident, $enable_caching:ident) => {
        mod $test_suite_name {
            use super::*;
            use fbinit::FacebookInit;

            #[fbinit::test]
            async fn test_simple_filenode_insert_and_get(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (reader, writer) = build_reader_writer($create_db()?);
                let reader = $enable_caching(reader);

                do_add_filenode(&ctx, &writer, root_first_filenode(), REPO_ZERO).await?;

                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::root(),
                    ONES_FNID,
                    REPO_ZERO,
                    root_first_filenode(),
                )
                .await?;

                assert_no_filenode(&ctx, &reader, &RepoPath::root(), TWOS_FNID, REPO_ZERO).await?;

                assert_no_filenode(&ctx, &reader, &RepoPath::root(), ONES_FNID, REPO_ONE).await?;

                Ok(())
            }

            #[fbinit::test]
            async fn test_insert_identical_in_batch(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (_reader, writer) = build_reader_writer($create_db()?);
                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![root_first_filenode(), root_first_filenode()],
                    REPO_ZERO,
                )
                .await?;
                Ok(())
            }

            #[fbinit::test]
            async fn test_filenode_insert_twice(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (_reader, writer) = build_reader_writer($create_db()?);
                do_add_filenode(&ctx, &writer, root_first_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, root_first_filenode(), REPO_ZERO).await?;
                Ok(())
            }

            #[fbinit::test]
            async fn test_insert_filenode_with_parent(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (reader, writer) = build_reader_writer($create_db()?);
                let reader = $enable_caching(reader);
                do_add_filenode(&ctx, &writer, root_first_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, root_second_filenode(), REPO_ZERO).await?;
                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::root(),
                    ONES_FNID,
                    REPO_ZERO,
                    root_first_filenode(),
                )
                .await?;
                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::root(),
                    TWOS_FNID,
                    REPO_ZERO,
                    root_second_filenode(),
                )
                .await?;
                Ok(())
            }

            #[fbinit::test]
            async fn test_insert_root_filenode_with_two_parents(
                fb: FacebookInit,
            ) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (reader, writer) = build_reader_writer($create_db()?);
                let reader = $enable_caching(reader);
                do_add_filenode(&ctx, &writer, root_first_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, root_second_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, root_merge_filenode(), REPO_ZERO).await?;
                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::root(),
                    THREES_FNID,
                    REPO_ZERO,
                    root_merge_filenode(),
                )
                .await?;
                Ok(())
            }

            #[fbinit::test]
            async fn test_insert_file_filenode(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (reader, writer) = build_reader_writer($create_db()?);
                let reader = $enable_caching(reader);
                do_add_filenode(&ctx, &writer, file_a_first_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, file_b_first_filenode(), REPO_ZERO).await?;

                assert_no_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::file("non-existent").unwrap(),
                    ONES_FNID,
                    REPO_ZERO,
                )
                .await?;
                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::file("a").unwrap(),
                    ONES_FNID,
                    REPO_ZERO,
                    file_a_first_filenode(),
                )
                .await?;
                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::file("b").unwrap(),
                    TWOS_FNID,
                    REPO_ZERO,
                    file_b_first_filenode(),
                )
                .await?;
                Ok(())
            }

            #[fbinit::test]
            async fn test_insert_different_repo(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (reader, writer) = build_reader_writer($create_db()?);
                let reader = $enable_caching(reader);
                do_add_filenode(&ctx, &writer, root_first_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, root_second_filenode(), REPO_ONE).await?;

                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::root(),
                    ONES_FNID,
                    REPO_ZERO,
                    root_first_filenode(),
                )
                .await?;

                assert_no_filenode(&ctx, &reader, &RepoPath::root(), ONES_FNID, REPO_ONE).await?;

                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::root(),
                    TWOS_FNID,
                    REPO_ONE,
                    root_second_filenode(),
                )
                .await?;
                Ok(())
            }

            #[fbinit::test]
            async fn test_insert_parent_and_child_in_same_batch(
                fb: FacebookInit,
            ) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (reader, writer) = build_reader_writer($create_db()?);
                let reader = $enable_caching(reader);

                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![root_first_filenode(), root_second_filenode()],
                    REPO_ZERO,
                )
                .await?;

                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::root(),
                    ONES_FNID,
                    REPO_ZERO,
                    root_first_filenode(),
                )
                .await?;

                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::root(),
                    TWOS_FNID,
                    REPO_ZERO,
                    root_second_filenode(),
                )
                .await?;
                Ok(())
            }

            #[fbinit::test]
            async fn insert_copied_file(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (reader, writer) = build_reader_writer($create_db()?);
                let reader = $enable_caching(reader);

                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![copied_from_filenode(), copied_filenode()],
                    REPO_ZERO,
                )
                .await?;
                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::file("copiedto").unwrap(),
                    TWOS_FNID,
                    REPO_ZERO,
                    copied_filenode(),
                )
                .await?;
                Ok(())
            }

            #[fbinit::test]
            async fn insert_same_copied_file(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (_reader, writer) = build_reader_writer($create_db()?);

                do_add_filenodes(&ctx, &writer, vec![copied_from_filenode()], REPO_ZERO).await?;
                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![copied_filenode(), copied_filenode()],
                    REPO_ZERO,
                )
                .await?;
                Ok(())
            }

            #[fbinit::test]
            async fn insert_copied_file_to_different_repo(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (reader, writer) = build_reader_writer($create_db()?);
                let reader = $enable_caching(reader);

                let copied = FilenodeInfo {
                    path: RepoPath::file("copiedto").unwrap(),
                    filenode: TWOS_FNID,
                    p1: None,
                    p2: None,
                    copyfrom: Some((RepoPath::file("copiedfrom").unwrap(), ONES_FNID)),
                    linknode: TWOS_CSID,
                };

                let notcopied = FilenodeInfo {
                    path: RepoPath::file("copiedto").unwrap(),
                    filenode: TWOS_FNID,
                    p1: None,
                    p2: None,
                    copyfrom: None,
                    linknode: TWOS_CSID,
                };

                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![copied_from_filenode(), copied.clone()],
                    REPO_ZERO,
                )
                .await?;

                do_add_filenodes(&ctx, &writer, vec![notcopied.clone()], REPO_ONE).await?;

                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::file("copiedto").unwrap(),
                    TWOS_FNID,
                    REPO_ZERO,
                    copied,
                )
                .await?;

                assert_filenode(
                    &ctx,
                    &reader,
                    &RepoPath::file("copiedto").unwrap(),
                    TWOS_FNID,
                    REPO_ONE,
                    notcopied,
                )
                .await?;
                Ok(())
            }

            #[fbinit::test]
            async fn get_all_filenodes_maybe_stale(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (reader, writer) = build_reader_writer($create_db()?);
                let reader = $enable_caching(reader);
                let root_filenodes = vec![
                    root_first_filenode(),
                    root_second_filenode(),
                    root_merge_filenode(),
                ];
                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![
                        root_first_filenode(),
                        root_second_filenode(),
                        root_merge_filenode(),
                    ],
                    REPO_ZERO,
                )
                .await?;
                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![file_a_first_filenode(), file_b_first_filenode()],
                    REPO_ZERO,
                )
                .await?;

                assert_all_filenodes(
                    &ctx,
                    &reader,
                    &RepoPath::RootPath,
                    REPO_ZERO,
                    &root_filenodes,
                )
                .await?;

                assert_all_filenodes(
                    &ctx,
                    &reader,
                    &RepoPath::file("a").unwrap(),
                    REPO_ZERO,
                    &vec![file_a_first_filenode()],
                )
                .await?;

                assert_all_filenodes(
                    &ctx,
                    &reader,
                    &RepoPath::file("b").unwrap(),
                    REPO_ZERO,
                    &vec![file_b_first_filenode()],
                )
                .await?;
                Ok(())
            }
        }
    };
}

fn create_unsharded() -> Result<Vec<Connection>, Error> {
    Ok(vec![build_shard()?])
}

fn create_sharded() -> Result<Vec<Connection>, Error> {
    (0..16).into_iter().map(|_| build_shard()).collect()
}

fn no_caching(reader: FilenodesReader) -> FilenodesReader {
    reader
}

fn with_caching(mut reader: FilenodesReader) -> FilenodesReader {
    reader.local_cache = LocalCache::Test(HashMapCache::new());
    reader
}

filenodes_tests!(uncached_unsharded_test, create_unsharded, no_caching);
filenodes_tests!(uncached_sharded_test, create_sharded, no_caching);

filenodes_tests!(cached_unsharded_test, create_unsharded, with_caching);
filenodes_tests!(cached_sharded_test, create_sharded, with_caching);