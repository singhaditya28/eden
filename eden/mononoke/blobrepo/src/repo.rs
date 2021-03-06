/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::utils::DangerousOverride;
use crate::bonsai_generation::{create_bonsai_changeset_object, save_bonsai_changeset_object};
use crate::derive_hg_manifest::derive_hg_manifest;
use crate::errors::*;
use crate::repo_commit::*;
use anyhow::{format_err, Context, Error};
use blobstore::{Blobstore, Loadable};
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_globalrev_mapping::{BonsaiGlobalrevMapping, BonsaisOrGlobalrevs};
use bonsai_hg_mapping::{BonsaiHgMapping, BonsaiHgMappingEntry, BonsaiOrHgChangesetIds};
use bookmarks::{
    self, Bookmark, BookmarkName, BookmarkPrefix, BookmarkUpdateLogEntry, BookmarkUpdateReason,
    Bookmarks, Freshness,
};
use cacheblob::LeaseOps;
use changeset_fetcher::{ChangesetFetcher, SimpleChangesetFetcher};
use changesets::{ChangesetEntry, ChangesetInsert, Changesets};
use cloned::cloned;
use context::CoreContext;
use failure_ext::{Compat, FutureFailureErrorExt, FutureFailureExt};
use filenodes::{FilenodeInfo, FilenodeResult, Filenodes};
use filestore::FilestoreConfig;
use futures::{
    compat::Future01CompatExt,
    future::{self as new_future, FutureExt as NewFutureExt, TryFutureExt},
};
use futures_ext::{spawn_future, try_boxfuture, BoxFuture, BoxStream, FutureExt, StreamExt};
use futures_old::future::{self, loop_fn, ok, Future, Loop};
use futures_old::stream::{self, futures_unordered, FuturesUnordered, Stream};
use futures_old::sync::oneshot;
use futures_old::IntoFuture;
use futures_stats::Timed;
use manifest::{Entry, Manifest, ManifestOps};
use maplit::hashmap;
use mercurial_mutation::HgMutationStore;
use mercurial_types::{
    blobs::{
        ChangesetMetadata, ContentBlobMeta, HgBlobChangeset, HgBlobEntry, HgBlobEnvelope,
        HgChangesetContent, UploadHgFileContents, UploadHgFileEntry, UploadHgNodeHash,
    },
    Globalrev, HgChangesetId, HgFileNodeId, HgManifestId, HgNodeHash, HgParents, RepoPath, Type,
};
use metaconfig_types::DerivedDataConfig;
use mononoke_types::{
    BlobstoreValue, BonsaiChangeset, ChangesetId, FileChange, Generation, MPath, RepositoryId,
    Timestamp,
};
use phases::{HeadsFetcher, Phases, SqlPhasesFactory};
use repo_blobstore::{RepoBlobstore, RepoBlobstoreArgs};
use scuba_ext::{ScubaSampleBuilder, ScubaSampleBuilderExt};
use slog::debug;
use stats::prelude::*;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    convert::From,
    sync::{Arc, Mutex},
    time::Duration,
};
use time_ext::DurationExt;
use topo_sort::sort_topological;
use tracing::{trace_args, EventId, Traced};
use uuid::Uuid;

define_stats! {
    prefix = "mononoke.blobrepo";
    get_bonsai_heads_maybe_stale: timeseries(Rate, Sum),
    get_bonsai_publishing_bookmarks_maybe_stale: timeseries(Rate, Sum),
    get_raw_hg_content: timeseries(Rate, Sum),
    get_changesets: timeseries(Rate, Sum),
    get_heads_maybe_stale: timeseries(Rate, Sum),
    changeset_exists: timeseries(Rate, Sum),
    changeset_exists_by_bonsai: timeseries(Rate, Sum),
    many_changesets_exists: timeseries(Rate, Sum),
    get_changeset_parents: timeseries(Rate, Sum),
    get_changeset_parents_by_bonsai: timeseries(Rate, Sum),
    get_hg_file_copy_from_blobstore: timeseries(Rate, Sum),
    get_hg_from_bonsai_changeset: timeseries(Rate, Sum),
    generate_hg_from_bonsai_changeset: timeseries(Rate, Sum),
    generate_hg_from_bonsai_total_latency_ms: histogram(100, 0, 10_000, Average; P 50; P 75; P 90; P 95; P 99),
    generate_hg_from_bonsai_single_latency_ms: histogram(100, 0, 10_000, Average; P 50; P 75; P 90; P 95; P 99),
    generate_hg_from_bonsai_generated_commit_num: histogram(1, 0, 20, Average; P 50; P 75; P 90; P 95; P 99),
    get_bookmark: timeseries(Rate, Sum),
    get_bookmarks_by_prefix_maybe_stale: timeseries(Rate, Sum),
    get_publishing_bookmarks_maybe_stale: timeseries(Rate, Sum),
    get_pull_default_bookmarks_maybe_stale: timeseries(Rate, Sum),
    get_bonsai_from_hg: timeseries(Rate, Sum),
    get_hg_bonsai_mapping: timeseries(Rate, Sum),
    update_bookmark_transaction: timeseries(Rate, Sum),
    get_linknode: timeseries(Rate, Sum),
    get_linknode_opt: timeseries(Rate, Sum),
    get_all_filenodes: timeseries(Rate, Sum),
    get_generation_number: timeseries(Rate, Sum),
    create_changeset: timeseries(Rate, Sum),
    create_changeset_compute_cf: timeseries("create_changeset.compute_changed_files"; Rate, Sum),
    create_changeset_expected_cf: timeseries("create_changeset.expected_changed_files"; Rate, Sum),
    create_changeset_cf_count: timeseries("create_changeset.changed_files_count"; Average, Sum),
}

pub struct BlobRepo {
    blobstore: RepoBlobstore,
    bookmarks: Arc<dyn Bookmarks>,
    filenodes: Arc<dyn Filenodes>,
    changesets: Arc<dyn Changesets>,
    bonsai_git_mapping: Arc<dyn BonsaiGitMapping>,
    bonsai_globalrev_mapping: Arc<dyn BonsaiGlobalrevMapping>,
    bonsai_hg_mapping: Arc<dyn BonsaiHgMapping>,
    hg_mutation_store: Arc<dyn HgMutationStore>,
    repoid: RepositoryId,
    // Returns new ChangesetFetcher that can be used by operation that work with commit graph
    // (for example, revsets).
    changeset_fetcher_factory:
        Arc<dyn Fn() -> Arc<dyn ChangesetFetcher + Send + Sync> + Send + Sync>,
    derived_data_lease: Arc<dyn LeaseOps>,
    filestore_config: FilestoreConfig,
    phases_factory: SqlPhasesFactory,
    derived_data_config: DerivedDataConfig,
    reponame: String,
}

impl BlobRepo {
    pub fn new(
        bookmarks: Arc<dyn Bookmarks>,
        blobstore_args: RepoBlobstoreArgs,
        filenodes: Arc<dyn Filenodes>,
        changesets: Arc<dyn Changesets>,
        bonsai_git_mapping: Arc<dyn BonsaiGitMapping>,
        bonsai_globalrev_mapping: Arc<dyn BonsaiGlobalrevMapping>,
        bonsai_hg_mapping: Arc<dyn BonsaiHgMapping>,
        hg_mutation_store: Arc<dyn HgMutationStore>,
        derived_data_lease: Arc<dyn LeaseOps>,
        filestore_config: FilestoreConfig,
        phases_factory: SqlPhasesFactory,
        derived_data_config: DerivedDataConfig,
        reponame: String,
    ) -> Self {
        let (blobstore, repoid) = blobstore_args.into_blobrepo_parts();

        let changeset_fetcher_factory = {
            cloned!(changesets, repoid);
            move || {
                let res: Arc<dyn ChangesetFetcher + Send + Sync> = Arc::new(
                    SimpleChangesetFetcher::new(changesets.clone(), repoid.clone()),
                );
                res
            }
        };

        BlobRepo {
            bookmarks,
            blobstore,
            filenodes,
            changesets,
            bonsai_git_mapping,
            bonsai_globalrev_mapping,
            bonsai_hg_mapping,
            hg_mutation_store,
            repoid,
            changeset_fetcher_factory: Arc::new(changeset_fetcher_factory),
            derived_data_lease,
            filestore_config,
            phases_factory,
            derived_data_config,
            reponame,
        }
    }

    /// Get Mercurial heads, which we approximate as publishing Bonsai Bookmarks.
    pub fn get_heads_maybe_stale(
        &self,
        ctx: CoreContext,
    ) -> impl Stream<Item = HgChangesetId, Error = Error> {
        STATS::get_heads_maybe_stale.add_value(1);
        self.get_bonsai_heads_maybe_stale(ctx.clone()).and_then({
            let repo = self.clone();
            move |cs| repo.get_hg_from_bonsai_changeset(ctx.clone(), cs)
        })
    }

    /// Get Bonsai changesets for Mercurial heads, which we approximate as Publishing Bonsai
    /// Bookmarks. Those will be served from cache, so they might be stale.
    pub fn get_bonsai_heads_maybe_stale(
        &self,
        ctx: CoreContext,
    ) -> impl Stream<Item = ChangesetId, Error = Error> {
        STATS::get_bonsai_heads_maybe_stale.add_value(1);
        self.bookmarks
            .list_publishing_by_prefix(
                ctx,
                &BookmarkPrefix::empty(),
                self.get_repoid(),
                Freshness::MaybeStale,
            )
            .map(|(_, cs_id)| cs_id)
    }

    /// List all publishing Bonsai bookmarks.
    pub fn get_bonsai_publishing_bookmarks_maybe_stale(
        &self,
        ctx: CoreContext,
    ) -> impl Stream<Item = (Bookmark, ChangesetId), Error = Error> {
        STATS::get_bonsai_publishing_bookmarks_maybe_stale.add_value(1);
        self.bookmarks.list_publishing_by_prefix(
            ctx,
            &BookmarkPrefix::empty(),
            self.repoid,
            Freshness::MaybeStale,
        )
    }

    /// Get bookmarks by prefix, they will be read from replica, so they might be stale.
    pub fn get_bonsai_bookmarks_by_prefix_maybe_stale(
        &self,
        ctx: CoreContext,
        prefix: &BookmarkPrefix,
        max: u64,
    ) -> impl Stream<Item = (Bookmark, ChangesetId), Error = Error> {
        STATS::get_bookmarks_by_prefix_maybe_stale.add_value(1);
        self.bookmarks.list_all_by_prefix(
            ctx.clone(),
            prefix,
            self.repoid,
            Freshness::MaybeStale,
            max,
        )
    }

    // TODO(stash): make it accept ChangesetId
    pub fn changeset_exists(
        &self,
        ctx: CoreContext,
        changesetid: HgChangesetId,
    ) -> BoxFuture<bool, Error> {
        STATS::changeset_exists.add_value(1);
        let changesetid = changesetid.clone();
        let repo = self.clone();
        let repoid = self.repoid.clone();

        self.get_bonsai_from_hg(ctx.clone(), changesetid)
            .and_then(move |maybebonsai| match maybebonsai {
                Some(bonsai) => repo
                    .changesets
                    .get(ctx, repoid, bonsai)
                    .map(|res| res.is_some())
                    .left_future(),
                None => Ok(false).into_future().right_future(),
            })
            .boxify()
    }

    pub fn changeset_exists_by_bonsai(
        &self,
        ctx: CoreContext,
        changesetid: ChangesetId,
    ) -> BoxFuture<bool, Error> {
        STATS::changeset_exists_by_bonsai.add_value(1);
        let changesetid = changesetid.clone();
        let repo = self.clone();
        let repoid = self.repoid.clone();

        repo.changesets
            .get(ctx, repoid, changesetid)
            .map(|res| res.is_some())
            .boxify()
    }

    // TODO(stash): make it accept ChangesetId
    pub fn get_changeset_parents(
        &self,
        ctx: CoreContext,
        changesetid: HgChangesetId,
    ) -> BoxFuture<Vec<HgChangesetId>, Error> {
        STATS::get_changeset_parents.add_value(1);
        let repo = self.clone();

        self.get_bonsai_cs_entry_or_fail(ctx.clone(), changesetid)
            .map(|bonsai| bonsai.parents)
            .and_then({
                cloned!(repo);
                move |bonsai_parents| {
                    future::join_all(bonsai_parents.into_iter().map(move |bonsai_parent| {
                        repo.get_hg_from_bonsai_changeset(ctx.clone(), bonsai_parent)
                    }))
                }
            })
            .boxify()
    }

    pub fn get_changeset_parents_by_bonsai(
        &self,
        ctx: CoreContext,
        changesetid: ChangesetId,
    ) -> impl Future<Item = Vec<ChangesetId>, Error = Error> {
        STATS::get_changeset_parents_by_bonsai.add_value(1);
        let repo = self.clone();
        let repoid = self.repoid.clone();

        repo.changesets
            .get(ctx, repoid, changesetid)
            .and_then(move |maybe_bonsai| {
                maybe_bonsai.ok_or(ErrorKind::BonsaiNotFound(changesetid).into())
            })
            .map(|bonsai| bonsai.parents)
    }

    fn get_bonsai_cs_entry_or_fail(
        &self,
        ctx: CoreContext,
        changesetid: HgChangesetId,
    ) -> impl Future<Item = ChangesetEntry, Error = Error> {
        let repoid = self.repoid.clone();
        let changesets = self.changesets.clone();

        self.get_bonsai_from_hg(ctx.clone(), changesetid)
            .and_then(move |maybebonsai| {
                maybebonsai.ok_or(ErrorKind::BonsaiMappingNotFound(changesetid).into())
            })
            .and_then(move |bonsai| {
                changesets
                    .get(ctx, repoid, bonsai)
                    .and_then(move |maybe_bonsai| {
                        maybe_bonsai.ok_or(ErrorKind::BonsaiNotFound(bonsai).into())
                    })
            })
    }

    pub fn get_bookmark(
        &self,
        ctx: CoreContext,
        name: &BookmarkName,
    ) -> BoxFuture<Option<HgChangesetId>, Error> {
        STATS::get_bookmark.add_value(1);
        self.bookmarks
            .get(ctx.clone(), name, self.repoid)
            .and_then({
                let repo = self.clone();
                move |cs_opt| match cs_opt {
                    None => future::ok(None).left_future(),
                    Some(cs) => repo
                        .get_hg_from_bonsai_changeset(ctx, cs)
                        .map(|cs| Some(cs))
                        .right_future(),
                }
            })
            .boxify()
    }

    pub fn get_bonsai_bookmark(
        &self,
        ctx: CoreContext,
        name: &BookmarkName,
    ) -> BoxFuture<Option<ChangesetId>, Error> {
        STATS::get_bookmark.add_value(1);
        self.bookmarks.get(ctx, name, self.repoid)
    }

    pub fn bonsai_git_mapping(&self) -> &Arc<dyn BonsaiGitMapping> {
        &self.bonsai_git_mapping
    }

    pub fn bonsai_globalrev_mapping(&self) -> &Arc<dyn BonsaiGlobalrevMapping> {
        &self.bonsai_globalrev_mapping
    }

    pub fn get_bonsai_from_globalrev(
        &self,
        globalrev: Globalrev,
    ) -> BoxFuture<Option<ChangesetId>, Error> {
        self.bonsai_globalrev_mapping
            .get_bonsai_from_globalrev(self.repoid, globalrev)
    }

    pub fn get_globalrev_from_bonsai(
        &self,
        bcs: ChangesetId,
    ) -> BoxFuture<Option<Globalrev>, Error> {
        self.bonsai_globalrev_mapping
            .get_globalrev_from_bonsai(self.repoid, bcs)
    }

    pub fn get_bonsai_globalrev_mapping(
        &self,
        bonsai_or_globalrev_ids: impl Into<BonsaisOrGlobalrevs>,
    ) -> BoxFuture<Vec<(ChangesetId, Globalrev)>, Error> {
        self.bonsai_globalrev_mapping
            .get(self.repoid, bonsai_or_globalrev_ids.into())
            .map(|result| {
                result
                    .into_iter()
                    .map(|entry| (entry.bcs_id, entry.globalrev))
                    .collect()
            })
            .boxify()
    }

    pub fn hg_mutation_store(&self) -> &Arc<dyn HgMutationStore> {
        &self.hg_mutation_store
    }

    pub fn list_bookmark_log_entries(
        &self,
        ctx: CoreContext,
        name: BookmarkName,
        max_rec: u32,
        offset: Option<u32>,
        freshness: Freshness,
    ) -> impl Stream<Item = (Option<ChangesetId>, BookmarkUpdateReason, Timestamp), Error = Error>
    {
        self.bookmarks.list_bookmark_log_entries(
            ctx.clone(),
            name,
            self.repoid,
            max_rec,
            offset,
            freshness,
        )
    }

    pub fn read_next_bookmark_log_entries(
        &self,
        ctx: CoreContext,
        id: u64,
        limit: u64,
        freshness: Freshness,
    ) -> impl Stream<Item = BookmarkUpdateLogEntry, Error = Error> {
        self.bookmarks
            .read_next_bookmark_log_entries(ctx, id, self.get_repoid(), limit, freshness)
    }

    pub fn count_further_bookmark_log_entries(
        &self,
        ctx: CoreContext,
        id: u64,
        exclude_reason: Option<BookmarkUpdateReason>,
    ) -> impl Future<Item = u64, Error = Error> {
        self.bookmarks.count_further_bookmark_log_entries(
            ctx,
            id,
            self.get_repoid(),
            exclude_reason,
        )
    }

    /// Get Pull-Default (Pull-Default is a Mercurial concept) bookmarks by prefix, they will be
    /// read from cache or a replica, so they might be stale.
    pub fn get_pull_default_bookmarks_maybe_stale(
        &self,
        ctx: CoreContext,
    ) -> impl Stream<Item = (Bookmark, HgChangesetId), Error = Error> {
        STATS::get_pull_default_bookmarks_maybe_stale.add_value(1);
        let stream = self.bookmarks.list_pull_default_by_prefix(
            ctx.clone(),
            &BookmarkPrefix::empty(),
            self.repoid,
            Freshness::MaybeStale,
        );
        to_hg_bookmark_stream(&self, &ctx, stream)
    }

    /// Get Publishing (Publishing is a Mercurial concept) bookmarks by prefix, they will be read
    /// from cache or a replica, so they might be stale.
    pub fn get_publishing_bookmarks_maybe_stale(
        &self,
        ctx: CoreContext,
    ) -> impl Stream<Item = (Bookmark, HgChangesetId), Error = Error> {
        STATS::get_publishing_bookmarks_maybe_stale.add_value(1);
        let stream = self.bookmarks.list_publishing_by_prefix(
            ctx.clone(),
            &BookmarkPrefix::empty(),
            self.repoid,
            Freshness::MaybeStale,
        );
        to_hg_bookmark_stream(&self, &ctx, stream)
    }

    /// Get bookmarks by prefix, they will be read from replica, so they might be stale.
    pub fn get_bookmarks_by_prefix_maybe_stale(
        &self,
        ctx: CoreContext,
        prefix: &BookmarkPrefix,
        max: u64,
    ) -> impl Stream<Item = (Bookmark, HgChangesetId), Error = Error> {
        STATS::get_bookmarks_by_prefix_maybe_stale.add_value(1);
        let stream = self.bookmarks.list_all_by_prefix(
            ctx.clone(),
            prefix,
            self.repoid,
            Freshness::MaybeStale,
            max,
        );
        to_hg_bookmark_stream(&self, &ctx, stream)
    }

    pub fn update_bookmark_transaction(&self, ctx: CoreContext) -> Box<dyn bookmarks::Transaction> {
        STATS::update_bookmark_transaction.add_value(1);
        self.bookmarks.create_transaction(ctx, self.repoid)
    }

    pub fn get_linknode_opt(
        &self,
        ctx: CoreContext,
        path: &RepoPath,
        node: HgFileNodeId,
    ) -> impl Future<Item = FilenodeResult<Option<HgChangesetId>>, Error = Error> {
        STATS::get_linknode_opt.add_value(1);
        self.get_filenode_opt(ctx, path, node).map(|filenode_res| {
            filenode_res.map(|filenode_opt| filenode_opt.map(|filenode| filenode.linknode))
        })
    }

    pub fn get_linknode(
        &self,
        ctx: CoreContext,
        path: &RepoPath,
        node: HgFileNodeId,
    ) -> impl Future<Item = FilenodeResult<HgChangesetId>, Error = Error> {
        STATS::get_linknode.add_value(1);
        self.get_filenode(ctx, path, node)
            .map(|filenode_res| filenode_res.map(|filenode| filenode.linknode))
    }

    pub fn get_filenode_opt(
        &self,
        ctx: CoreContext,
        path: &RepoPath,
        node: HgFileNodeId,
    ) -> impl Future<Item = FilenodeResult<Option<FilenodeInfo>>, Error = Error> {
        let path = path.clone();
        self.filenodes
            .get_filenode(ctx, &path, node, self.repoid)
            .map(FilenodeResult::Present)
    }

    pub fn get_filenode(
        &self,
        ctx: CoreContext,
        path: &RepoPath,
        node: HgFileNodeId,
    ) -> impl Future<Item = FilenodeResult<FilenodeInfo>, Error = Error> {
        self.get_filenode_opt(ctx, path, node).and_then({
            cloned!(path);
            move |filenode_res| match filenode_res {
                FilenodeResult::Present(maybe_filenode) => {
                    let filenode = maybe_filenode
                        .ok_or_else(|| Error::from(ErrorKind::MissingFilenode(path, node)))?;
                    Ok(FilenodeResult::Present(filenode))
                }
                FilenodeResult::Disabled => Ok(FilenodeResult::Disabled),
            }
        })
    }

    pub fn get_filenode_from_envelope(
        &self,
        ctx: CoreContext,
        path: &RepoPath,
        node: HgFileNodeId,
        linknode: HgChangesetId,
    ) -> impl Future<Item = FilenodeInfo, Error = Error> {
        node.load(ctx, &self.blobstore)
            .with_context({
                cloned!(path);
                move || format!("While fetching filenode for {} {}", path, node)
            })
            .from_err()
            .and_then({
                cloned!(path, linknode);
                move |envelope| {
                    let (p1, p2) = envelope.parents();
                    let copyfrom = envelope
                        .get_copy_info()
                        .with_context({
                            cloned!(path);
                            move || format!("While parsing copy information for {} {}", path, node)
                        })?
                        .map(|(path, node)| (RepoPath::FilePath(path), node));
                    Ok(FilenodeInfo {
                        filenode: node,
                        p1,
                        p2,
                        copyfrom,
                        linknode,
                    })
                }
            })
    }

    pub fn get_all_filenodes_maybe_stale(
        &self,
        ctx: CoreContext,
        path: RepoPath,
    ) -> BoxFuture<Vec<FilenodeInfo>, Error> {
        STATS::get_all_filenodes.add_value(1);
        self.filenodes
            .get_all_filenodes_maybe_stale(ctx, &path, self.repoid)
    }

    pub fn get_bonsai_from_hg(
        &self,
        ctx: CoreContext,
        hg_cs_id: HgChangesetId,
    ) -> BoxFuture<Option<ChangesetId>, Error> {
        STATS::get_bonsai_from_hg.add_value(1);
        self.bonsai_hg_mapping
            .get_bonsai_from_hg(ctx, self.repoid, hg_cs_id)
    }

    // Returns only the mapping for valid changests that are known to the server.
    // For Bonsai -> Hg conversion, missing Hg changesets will be derived (so all Bonsais will be
    // in the output).
    // For Hg -> Bonsai conversion, missing Bonsais will not be returned, since they cannot be
    // derived from Hg Changesets.
    pub fn get_hg_bonsai_mapping(
        &self,
        ctx: CoreContext,
        bonsai_or_hg_cs_ids: impl Into<BonsaiOrHgChangesetIds>,
    ) -> BoxFuture<Vec<(HgChangesetId, ChangesetId)>, Error> {
        STATS::get_hg_bonsai_mapping.add_value(1);

        let bonsai_or_hg_cs_ids = bonsai_or_hg_cs_ids.into();
        let fetched_from_mapping = self
            .bonsai_hg_mapping
            .get(ctx.clone(), self.repoid, bonsai_or_hg_cs_ids.clone())
            .map(|result| {
                result
                    .into_iter()
                    .map(|entry| (entry.hg_cs_id, entry.bcs_id))
                    .collect::<Vec<_>>()
            })
            .boxify();

        use BonsaiOrHgChangesetIds::*;
        match bonsai_or_hg_cs_ids {
            Bonsai(bonsais) => fetched_from_mapping
                .and_then({
                    let repo = self.clone();
                    move |hg_bonsai_list| {
                        // If a bonsai commit doesn't exist in the bonsai_hg_mapping,
                        // that might mean two things: 1) Bonsai commit just doesn't exist
                        // 2) Bonsai commit exists but hg changesets weren't generated for it
                        // Normally the callers of get_hg_bonsai_mapping would expect that hg
                        // changesets will be lazily generated, so the
                        // code below explicitly checks if a commit exists and if yes then
                        // generates hg changeset for it.
                        let mapping: HashMap<_, _> = hg_bonsai_list
                            .iter()
                            .map(|(hg_id, bcs_id)| (bcs_id, hg_id))
                            .collect();
                        let mut notfound = vec![];
                        for b in bonsais {
                            if !mapping.contains_key(&b) {
                                notfound.push(b);
                            }
                        }
                        repo.changesets
                            .get_many(ctx.clone(), repo.get_repoid(), notfound.clone())
                            .and_then(move |existing| {
                                let existing: HashSet<_> =
                                    existing.into_iter().map(|entry| entry.cs_id).collect();

                                futures_unordered(
                                    notfound
                                        .into_iter()
                                        .filter(|cs_id| existing.contains(cs_id))
                                        .map(move |bcs_id| {
                                            repo.get_hg_from_bonsai_changeset(ctx.clone(), bcs_id)
                                                .map(move |hg_cs_id| (hg_cs_id, bcs_id))
                                        }),
                                )
                                .collect()
                            })
                            .map(move |mut newmapping| {
                                newmapping.extend(hg_bonsai_list);
                                newmapping
                            })
                    }
                })
                .boxify(),
            Hg(_) => fetched_from_mapping,
        }
        // TODO(stash, luk): T37303879 also need to check that entries exist in changeset table
    }

    // Returns the generation number of a changeset
    // note: it returns Option because changeset might not exist
    pub fn get_generation_number(
        &self,
        ctx: CoreContext,
        cs: ChangesetId,
    ) -> impl Future<Item = Option<Generation>, Error = Error> {
        STATS::get_generation_number.add_value(1);
        let repo = self.clone();
        let repoid = self.repoid.clone();
        repo.changesets
            .get(ctx, repoid, cs)
            .map(|res| res.map(|res| Generation::new(res.gen)))
    }

    pub fn get_changeset_fetcher(&self) -> Arc<dyn ChangesetFetcher> {
        (self.changeset_fetcher_factory)()
    }

    pub fn blobstore(&self) -> &RepoBlobstore {
        &self.blobstore
    }

    pub fn get_blobstore(&self) -> RepoBlobstore {
        self.blobstore.clone()
    }

    pub fn filestore_config(&self) -> FilestoreConfig {
        self.filestore_config
    }

    pub fn get_repoid(&self) -> RepositoryId {
        self.repoid
    }

    pub fn get_filenodes(&self) -> Arc<dyn Filenodes> {
        self.filenodes.clone()
    }

    pub fn get_phases(&self) -> Arc<dyn Phases> {
        self.phases_factory.get_phases(
            self.repoid,
            (self.changeset_fetcher_factory)(),
            self.get_heads_fetcher(),
        )
    }

    pub fn name(&self) -> &String {
        &self.reponame
    }

    pub fn get_heads_fetcher(&self) -> HeadsFetcher {
        let this = self.clone();
        Arc::new(move |ctx: &CoreContext| {
            this.get_bonsai_heads_maybe_stale(ctx.clone())
                .collect()
                .compat()
                .boxed()
        })
    }

    pub fn get_bonsai_hg_mapping(&self) -> Arc<dyn BonsaiHgMapping> {
        self.bonsai_hg_mapping.clone()
    }

    pub fn get_bookmarks_object(&self) -> Arc<dyn Bookmarks> {
        self.bookmarks.clone()
    }

    pub fn get_phases_factory(&self) -> &SqlPhasesFactory {
        &self.phases_factory
    }

    pub fn get_changesets_object(&self) -> Arc<dyn Changesets> {
        self.changesets.clone()
    }

    pub fn get_derived_data_config(&self) -> &DerivedDataConfig {
        &self.derived_data_config
    }

    fn store_file_change(
        &self,
        ctx: CoreContext,
        p1: Option<HgFileNodeId>,
        p2: Option<HgFileNodeId>,
        path: &MPath,
        change: &FileChange,
        copy_from: Option<(MPath, HgFileNodeId)>,
    ) -> impl Future<Item = HgBlobEntry, Error = Error> + Send {
        // If we produced a hg change that has copy info, then the Bonsai should have copy info
        // too. However, we could have Bonsai copy info without having copy info in the hg change
        // if we stripped it out to produce a hg changeset for an Octopus merge and the copy info
        // references a step-parent (i.e. neither p1, not p2).
        if copy_from.is_some() {
            assert!(change.copy_from().is_some());
        }

        // we can reuse same HgFileNodeId if we have only one parent with same
        // file content but different type (Regular|Executable)
        match (p1, p2) {
            (Some(parent), None) | (None, Some(parent)) => {
                let store = self.get_blobstore().boxed();
                cloned!(ctx, change, path);
                parent
                    .load(ctx.clone(), &store)
                    .from_err()
                    .map(move |parent_envelope| {
                        if parent_envelope.content_id() == change.content_id()
                            && change.copy_from().is_none()
                        {
                            Some(HgBlobEntry::new(
                                store,
                                path.basename().clone(),
                                parent.into_nodehash(),
                                Type::File(change.file_type()),
                            ))
                        } else {
                            None
                        }
                    })
                    .right_future()
            }
            _ => future::ok(None).left_future(),
        }
        .and_then({
            let repo = self.clone();
            cloned!(path, change);
            move |maybe_entry| match maybe_entry {
                Some(entry) => future::ok(entry).left_future(),
                None => {
                    // Mercurial has complicated logic of finding file parents, especially
                    // if a file was also copied/moved.
                    // See mercurial/localrepo.py:_filecommit(). We have to replicate this
                    // logic in Mononoke.
                    // TODO(stash): T45618931 replicate all the cases from _filecommit()

                    let parents_fut = if let Some((ref copy_from_path, _)) = copy_from {
                        if copy_from_path != &path && p1.is_some() && p2.is_none() {
                            // This case can happen if a file existed in it's parent
                            // but it was copied over:
                            // ```
                            // echo 1 > 1 && echo 2 > 2 && hg ci -A -m first
                            // hg cp 2 1 --force && hg ci -m second
                            // # File '1' has both p1 and copy from.
                            // ```
                            // In that case Mercurial discards p1 i.e. `hg log` will
                            // use copy from revision as a parent. Arguably not the best
                            // decision, but we have to keep it.
                            ok((None, None)).left_future()
                        } else {
                            ok((p1, p2)).left_future()
                        }
                    } else if p1.is_none() {
                        ok((p2, None)).left_future()
                    } else if p2.is_some() {
                        crate::file_history::check_if_related(
                            ctx.clone(),
                            repo.clone(),
                            p1.unwrap(),
                            p2.unwrap(),
                            path.clone(),
                        )
                        .map(move |res| {
                            use crate::file_history::FilenodesRelatedResult::*;

                            match res {
                                Unrelated => (p1, p2),
                                FirstAncestorOfSecond => (p2, None),
                                SecondAncestorOfFirst => (p1, None),
                            }
                        })
                        .right_future()
                    } else {
                        ok((p1, p2)).left_future()
                    };

                    parents_fut
                        .and_then({
                            move |(p1, p2)| {
                                let upload_entry = UploadHgFileEntry {
                                    upload_node_id: UploadHgNodeHash::Generate,
                                    contents: UploadHgFileContents::ContentUploaded(
                                        ContentBlobMeta {
                                            id: change.content_id(),
                                            size: change.size(),
                                            copy_from: copy_from.clone(),
                                        },
                                    ),
                                    file_type: change.file_type(),
                                    p1,
                                    p2,
                                    path: path.clone(),
                                };
                                match upload_entry.upload(ctx, repo.get_blobstore().boxed()) {
                                    Ok((_, upload_fut)) => {
                                        upload_fut.map(move |(entry, _)| entry).left_future()
                                    }
                                    Err(err) => return future::err(err).right_future(),
                                }
                            }
                        })
                        .right_future()
                }
            }
        })
    }

    /// Check if adding a single path to manifest would cause case-conflict
    ///
    /// Implementation traverses manifest and checks if correspoinding path element is present,
    /// if path element is not present, it lowercases current path element and checks if it
    /// collides with any existing elements inside manifest. if so it also needs to check that
    /// child manifest contains this entry, because it might have been removed.
    pub fn check_case_conflict_in_manifest(
        &self,
        ctx: CoreContext,
        parent_mf_id: HgManifestId,
        child_mf_id: HgManifestId,
        path: MPath,
    ) -> impl Future<Item = Option<MPath>, Error = Error> {
        let repo = self.clone();
        let child_mf_id = child_mf_id.clone();
        parent_mf_id
            .load(ctx.clone(), &self.blobstore)
            .from_err()
            .and_then(move |mf| {
                loop_fn(
                    (None, mf, path.into_iter()),
                    move |(cur_path, mf, mut elements): (Option<MPath>, _, _)| {
                        let element = match elements.next() {
                            None => return future::ok(Loop::Break(None)).boxify(),
                            Some(element) => element,
                        };

                        match mf.lookup(&element) {
                            Some(entry) => {
                                let cur_path = MPath::join_opt_element(cur_path.as_ref(), &element);
                                match entry {
                                    Entry::Leaf(..) => future::ok(Loop::Break(None)).boxify(),
                                    Entry::Tree(manifest_id) => manifest_id
                                        .load(ctx.clone(), repo.blobstore())
                                        .from_err()
                                        .map(move |mf| {
                                            Loop::Continue((Some(cur_path), mf, elements))
                                        })
                                        .boxify(),
                                }
                            }
                            None => {
                                let element_utf8 = String::from_utf8(Vec::from(element.as_ref()));
                                let mut potential_conflicts = vec![];
                                // Find all entries in the manifests that can potentially be a conflict.
                                // Entry can potentially be a conflict if its lowercased version
                                // is the same as lowercased version of the current element

                                for (basename, _) in mf.list() {
                                    let path =
                                        MPath::join_element_opt(cur_path.as_ref(), Some(&basename));
                                    match (&element_utf8, std::str::from_utf8(basename.as_ref())) {
                                        (Ok(ref element), Ok(ref basename)) => {
                                            if basename.to_lowercase() == element.to_lowercase() {
                                                potential_conflicts.extend(path);
                                            }
                                        }
                                        _ => (),
                                    }
                                }

                                // For each potential conflict we need to check if it's present in
                                // child manifest. If it is, then we've got a conflict, otherwise
                                // this has been deleted and it's no longer a conflict.
                                child_mf_id
                                    .find_entries(
                                        ctx.clone(),
                                        repo.get_blobstore(),
                                        potential_conflicts,
                                    )
                                    .collect()
                                    .map(|entries| {
                                        // NOTE: We flatten here because we cannot have a conflict
                                        // at the root.
                                        Loop::Break(entries.into_iter().next().and_then(|x| x.0))
                                    })
                                    .boxify()
                            }
                        }
                    },
                )
            })
    }

    pub fn get_manifest_from_bonsai(
        &self,
        ctx: CoreContext,
        bcs: BonsaiChangeset,
        parent_manifests: Vec<HgManifestId>,
    ) -> BoxFuture<HgManifestId, Error> {
        let repo = self.clone();
        let event_id = EventId::new();

        // NOTE: We ignore further parents beyond p1 and p2 for the purposed of tracking copy info
        // or filenode parents. This is because hg supports just 2 parents at most, so we track
        // copy info & filenode parents relative to the first 2 parents, then ignore other parents.

        let (manifest_p1, manifest_p2) = {
            let mut manifests = parent_manifests.iter();
            (manifests.next().copied(), manifests.next().copied())
        };

        let (p1, p2) = {
            let mut parents = bcs.parents();
            let p1 = parents.next();
            let p2 = parents.next();
            (p1, p2)
        };

        // paths *modified* by changeset or *copied from parents*
        let mut p1_paths = Vec::new();
        let mut p2_paths = Vec::new();
        for (path, file_change) in bcs.file_changes() {
            if let Some(file_change) = file_change {
                if let Some((copy_path, bcsid)) = file_change.copy_from() {
                    if Some(bcsid) == p1.as_ref() {
                        p1_paths.push(copy_path.clone());
                    }
                    if Some(bcsid) == p2.as_ref() {
                        p2_paths.push(copy_path.clone());
                    }
                };
                p1_paths.push(path.clone());
                p2_paths.push(path.clone());
            }
        }

        let resolve_paths = {
            cloned!(ctx, self.blobstore);
            move |maybe_manifest_id: Option<HgManifestId>, paths| match maybe_manifest_id {
                None => future::ok(HashMap::new()).right_future(),
                Some(manifest_id) => manifest_id
                    .find_entries(ctx.clone(), blobstore.clone(), paths)
                    .filter_map(|(path, entry)| Some((path?, entry.into_leaf()?.1)))
                    .collect_to::<HashMap<MPath, HgFileNodeId>>()
                    .left_future(),
            }
        };

        // TODO:
        // `derive_manifest` already provides parents for newly created files, so we
        // can remove **all** lookups to files from here, and only leave lookups for
        // files that were copied (i.e bonsai changes that contain `copy_path`)
        let store_file_changes = (
            resolve_paths(manifest_p1, p1_paths),
            resolve_paths(manifest_p2, p2_paths),
        )
            .into_future()
            .traced_with_id(
                &ctx.trace(),
                "generate_hg_manifest::traverse_parents",
                trace_args! {},
                event_id,
            )
            .and_then({
                cloned!(ctx, repo);
                move |(p1s, p2s)| {
                    let file_changes: Vec<_> = bcs
                        .file_changes()
                        .map(|(path, file_change)| (path.clone(), file_change.cloned()))
                        .collect();
                    stream::iter_ok(file_changes)
                        .map({
                            cloned!(ctx);
                            move |(path, file_change)| match file_change {
                                None => future::ok((path, None)).left_future(),
                                Some(file_change) => {
                                    let copy_from =
                                        file_change.copy_from().and_then(|(copy_path, bcsid)| {
                                            if Some(bcsid) == p1.as_ref() {
                                                p1s.get(copy_path)
                                                    .map(|id| (copy_path.clone(), *id))
                                            } else if Some(bcsid) == p2.as_ref() {
                                                p2s.get(copy_path)
                                                    .map(|id| (copy_path.clone(), *id))
                                            } else {
                                                None
                                            }
                                        });
                                    repo.store_file_change(
                                        ctx.clone(),
                                        p1s.get(&path).cloned(),
                                        p2s.get(&path).cloned(),
                                        &path,
                                        &file_change,
                                        copy_from,
                                    )
                                    .map(move |entry| (path, Some(entry)))
                                    .right_future()
                                }
                            }
                        })
                        .buffer_unordered(100)
                        .collect()
                        .traced_with_id(
                            &ctx.trace(),
                            "generate_hg_manifest::store_file_changes",
                            trace_args! {},
                            event_id,
                        )
                }
            });

        let create_manifest = {
            cloned!(ctx, repo);
            move |changes| {
                derive_hg_manifest(
                    ctx.clone(),
                    repo.get_blobstore().boxed(),
                    parent_manifests,
                    changes,
                )
                .traced_with_id(
                    &ctx.trace(),
                    "generate_hg_manifest::create_manifest",
                    trace_args! {},
                    event_id,
                )
            }
        };

        store_file_changes
            .and_then(create_manifest)
            .traced_with_id(
                &ctx.trace(),
                "generate_hg_manifest",
                trace_args! {},
                event_id,
            )
            .boxify()
    }

    pub fn get_hg_from_bonsai_changeset(
        &self,
        ctx: CoreContext,
        bcs_id: ChangesetId,
    ) -> impl Future<Item = HgChangesetId, Error = Error> + Send {
        STATS::get_hg_from_bonsai_changeset.add_value(1);
        self.get_hg_from_bonsai_changeset_with_impl(ctx, bcs_id)
            .map(|(hg_cs_id, generated_commit_num)| {
                STATS::generate_hg_from_bonsai_generated_commit_num
                    .add_value(generated_commit_num as i64);
                hg_cs_id
            })
            .timed(move |stats, _| {
                STATS::generate_hg_from_bonsai_total_latency_ms
                    .add_value(stats.completion_time.as_millis_unchecked() as i64);
                Ok(())
            })
    }

    pub fn get_derived_data_lease_ops(&self) -> Arc<dyn LeaseOps> {
        self.derived_data_lease.clone()
    }

    fn generate_lease_key(&self, bcs_id: &ChangesetId) -> String {
        let repoid = self.get_repoid();
        format!("repoid.{}.hg-changeset.{}", repoid.id(), bcs_id)
    }

    fn take_hg_generation_lease(
        &self,
        ctx: CoreContext,
        bcs_id: ChangesetId,
    ) -> impl Future<Item = Option<HgChangesetId>, Error = Error> + Send {
        let key = self.generate_lease_key(&bcs_id);
        let repoid = self.get_repoid();

        cloned!(self.bonsai_hg_mapping, self.derived_data_lease);
        let repo = self.clone();

        let backoff_ms = 200;
        loop_fn(backoff_ms, move |mut backoff_ms| {
            cloned!(ctx, key);
            derived_data_lease
                .try_add_put_lease(&key)
                .or_else(|_| Ok(false))
                .and_then({
                    cloned!(bcs_id, bonsai_hg_mapping, repo);
                    move |leased| {
                        let maybe_hg_cs =
                            bonsai_hg_mapping.get_hg_from_bonsai(ctx.clone(), repoid, bcs_id);
                        if leased {
                            maybe_hg_cs
                                .and_then(move |maybe_hg_cs| match maybe_hg_cs {
                                    Some(hg_cs) => repo
                                        .release_hg_generation_lease(bcs_id)
                                        .then(move |_| Ok(Loop::Break(Some(hg_cs))))
                                        .left_future(),
                                    None => future::ok(Loop::Break(None)).right_future(),
                                })
                                .left_future()
                        } else {
                            maybe_hg_cs
                                .and_then(move |maybe_hg_cs_id| match maybe_hg_cs_id {
                                    Some(hg_cs_id) => {
                                        future::ok(Loop::Break(Some(hg_cs_id))).left_future()
                                    }
                                    None => {
                                        tokio::time::delay_for(Duration::from_millis(backoff_ms))
                                            .then(|_| new_future::ready(Ok(())))
                                            .compat()
                                            .then(move |_: Result<(), Error>| {
                                                backoff_ms *= 2;
                                                if backoff_ms >= 1000 {
                                                    backoff_ms = 1000;
                                                }
                                                Ok(Loop::Continue(backoff_ms))
                                            })
                                            .right_future()
                                    }
                                })
                                .right_future()
                        }
                    }
                })
        })
    }

    fn renew_hg_generation_lease_forever(
        &self,
        ctx: CoreContext,
        bcs_id: ChangesetId,
        done: BoxFuture<(), ()>,
    ) {
        let key = self.generate_lease_key(&bcs_id);
        self.derived_data_lease.renew_lease_until(ctx, &key, done)
    }

    fn release_hg_generation_lease(
        &self,
        bcs_id: ChangesetId,
    ) -> impl Future<Item = (), Error = ()> + Send {
        let key = self.generate_lease_key(&bcs_id);
        self.derived_data_lease.release_lease(&key)
    }

    fn generate_hg_changeset(
        &self,
        ctx: CoreContext,
        bcs_id: ChangesetId,
        bcs: BonsaiChangeset,
        parents: Vec<HgBlobChangeset>,
    ) -> impl Future<Item = HgChangesetId, Error = Error> + Send {
        let parent_manifests = parents.iter().map(|p| p.manifestid()).collect();

        // NOTE: We're special-casing the first 2 parents here, since that's all Mercurial
        // supports. Producing the Manifest (in get_manifest_from_bonsai) will consider all
        // parents, but everything else is only presented with the first 2 parents, because that's
        // all Mercurial knows about for now. This lets us produce a meaningful Hg changeset for a
        // Bonsai changeset with > 2 parents (which might be one we imported from Git).
        let mut parents = parents.into_iter();
        let p1 = parents.next();
        let p2 = parents.next();

        let p1_hash = p1.as_ref().map(|p1| p1.get_changeset_id());
        let p2_hash = p2.as_ref().map(|p2| p2.get_changeset_id());

        let mf_p1 = p1.map(|p| p.manifestid());
        let mf_p2 = p2.map(|p| p.manifestid());

        let hg_parents = HgParents::new(
            p1_hash.map(|h| h.into_nodehash()),
            p2_hash.map(|h| h.into_nodehash()),
        );

        // Keep a record of any parents for now (i.e. > 2 parents). We'll store those in extras.
        let step_parents = parents;

        let repo = self.clone();
        repo.get_manifest_from_bonsai(ctx.clone(), bcs.clone(), parent_manifests)
            .and_then({
                cloned!(ctx, repo);
                move |manifest_id| {
                compute_changed_files(ctx, repo, manifest_id.clone(), mf_p1, mf_p2)
                    .map(move |files| {
                        (manifest_id, files)
                    })

            }})
            // create changeset
            .and_then({
                cloned!(ctx, repo, bcs);
                move |(manifest_id, files)| {
                    let mut metadata = ChangesetMetadata {
                        user: bcs.author().to_string(),
                        time: *bcs.author_date(),
                        extra: bcs.extra()
                            .map(|(k, v)| {
                                (k.as_bytes().to_vec(), v.to_vec())
                            })
                            .collect(),
                        comments: bcs.message().to_string(),
                    };

                    metadata.record_step_parents(step_parents.into_iter().map(|blob| blob.get_changeset_id()));

                    let content = HgChangesetContent::new_from_parts(
                        hg_parents,
                        manifest_id,
                        metadata,
                        files,
                    );
                    let cs = try_boxfuture!(HgBlobChangeset::new(content));
                    let cs_id = cs.get_changeset_id();

                    cs.save(ctx.clone(), repo.blobstore.clone())
                        .and_then({
                            cloned!(ctx, repo);
                            move |_| repo.bonsai_hg_mapping.add(
                                ctx,
                                BonsaiHgMappingEntry {
                                    repo_id: repo.get_repoid(),
                                    hg_cs_id: cs_id,
                                    bcs_id,
                                },
                            )
                        })
                        .map(move |_| cs_id)
                        .boxify()
                }
            })
            .traced(
                &ctx.trace(),
                "generate_hg_changeset",
                trace_args! {"changeset" => bcs_id.to_hex().to_string()},
            )
            .timed(move |stats, _| {
                STATS::generate_hg_from_bonsai_single_latency_ms
                    .add_value(stats.completion_time.as_millis_unchecked() as i64);
                Ok(())
            })
    }

    // Converts Bonsai changesets to hg changesets. It either fetches hg changeset id from
    // bonsai-hg mapping or it generates hg changeset and puts hg changeset id in bonsai-hg mapping.
    // Note that it generates parent hg changesets first.
    // This function takes care of making sure the same changeset is not generated at the same time
    // by taking leases. It also avoids using recursion to prevents stackoverflow
    pub fn get_hg_from_bonsai_changeset_with_impl(
        &self,
        ctx: CoreContext,
        bcs_id: ChangesetId,
    ) -> impl Future<Item = (HgChangesetId, usize), Error = Error> + Send {
        // Finds parent bonsai commits which do not have corresponding hg changeset generated
        // Avoids using recursion
        fn find_toposorted_bonsai_cs_with_no_hg_cs_generated(
            ctx: CoreContext,
            repo: BlobRepo,
            bcs_id: ChangesetId,
            bonsai_hg_mapping: Arc<dyn BonsaiHgMapping>,
        ) -> impl Future<Item = Vec<BonsaiChangeset>, Error = Error> {
            let mut queue = VecDeque::new();
            let mut visited: HashSet<ChangesetId> = HashSet::new();
            visited.insert(bcs_id);
            queue.push_back(bcs_id);

            let repoid = repo.repoid;
            loop_fn(
                (queue, vec![], visited),
                move |(mut queue, mut commits_to_generate, mut visited)| {
                    cloned!(ctx, repo);
                    match queue.pop_front() {
                        Some(bcs_id) => bonsai_hg_mapping
                            .get_hg_from_bonsai(ctx.clone(), repoid, bcs_id)
                            .and_then(move |maybe_hg| match maybe_hg {
                                Some(_hg_cs_id) => future::ok(Loop::Continue((
                                    queue,
                                    commits_to_generate,
                                    visited,
                                )))
                                .left_future(),
                                None => bcs_id
                                    .load(ctx.clone(), repo.blobstore())
                                    .from_err()
                                    .map(move |bcs| {
                                        commits_to_generate.push(bcs.clone());
                                        queue.extend(bcs.parents().filter(|p| visited.insert(*p)));
                                        Loop::Continue((queue, commits_to_generate, visited))
                                    })
                                    .right_future(),
                            })
                            .left_future(),
                        None => future::ok(Loop::Break(commits_to_generate)).right_future(),
                    }
                },
            )
            .map(|changesets| {
                let mut graph = hashmap! {};
                let mut id_to_bcs = hashmap! {};
                for cs in changesets {
                    graph.insert(cs.get_changeset_id(), cs.parents().collect());
                    id_to_bcs.insert(cs.get_changeset_id(), cs);
                }
                sort_topological(&graph)
                    .expect("commit graph has cycles!")
                    .into_iter()
                    .map(|cs_id| id_to_bcs.remove(&cs_id))
                    .filter_map(|x| x)
                    .collect()
            })
        }

        // Panics if changeset not found
        fn fetch_hg_changeset_from_mapping(
            ctx: CoreContext,
            repo: BlobRepo,
            bcs_id: ChangesetId,
        ) -> impl Future<Item = HgBlobChangeset, Error = Error> {
            let bonsai_hg_mapping = repo.bonsai_hg_mapping.clone();
            let repoid = repo.repoid;

            bonsai_hg_mapping
                .get_hg_from_bonsai(ctx.clone(), repoid, bcs_id)
                .and_then(move |maybe_hg| match maybe_hg {
                    Some(hg_cs_id) => hg_cs_id.load(ctx, repo.blobstore()).from_err(),
                    None => panic!("hg changeset must be generated already"),
                })
        }

        // Panics if parent hg changesets are not generated
        // Returns whether a commit was generated or not
        fn generate_single_hg_changeset(
            ctx: CoreContext,
            repo: BlobRepo,
            bcs: BonsaiChangeset,
        ) -> impl Future<Item = (HgChangesetId, bool), Error = Error> {
            let bcs_id = bcs.get_changeset_id();

            repo.take_hg_generation_lease(ctx.clone(), bcs_id.clone())
                .traced(
                    &ctx.trace(),
                    "create_hg_from_bonsai::wait_for_lease",
                    trace_args! {},
                )
                .and_then({
                    cloned!(ctx, repo);
                    move |maybe_hg_cs_id| {
                        match maybe_hg_cs_id {
                            Some(hg_cs_id) => future::ok((hg_cs_id, false)).left_future(),
                            None => {
                                // We have the lease
                                STATS::generate_hg_from_bonsai_changeset.add_value(1);

                                let mut hg_parents = vec![];
                                for p in bcs.parents() {
                                    hg_parents.push(fetch_hg_changeset_from_mapping(
                                        ctx.clone(),
                                        repo.clone(),
                                        p,
                                    ));
                                }

                                future::join_all(hg_parents)
                                    .and_then({
                                        cloned!(repo);
                                        move |hg_parents| {
                                            let (sender, receiver) = oneshot::channel();

                                            repo.renew_hg_generation_lease_forever(
                                                ctx.clone(),
                                                bcs_id,
                                                receiver.map_err(|_| ()).boxify(),
                                            );

                                            repo.generate_hg_changeset(
                                                ctx.clone(),
                                                bcs_id,
                                                bcs,
                                                hg_parents,
                                            )
                                            .then(
                                                move |res| {
                                                    let _ = sender.send(());
                                                    res
                                                },
                                            )
                                        }
                                    })
                                    .map(|hg_cs_id| (hg_cs_id, true))
                                    .right_future()
                            }
                        }
                    }
                })
                .timed(move |stats, _| {
                    ctx.scuba()
                        .clone()
                        .add_future_stats(&stats)
                        .log_with_msg("Generating hg changeset", Some(format!("{}", bcs_id)));
                    Ok(())
                })
        }

        let repo = self.clone();

        cloned!(self.bonsai_hg_mapping, self.repoid);
        find_toposorted_bonsai_cs_with_no_hg_cs_generated(
            ctx.clone(),
            repo.clone(),
            bcs_id.clone(),
            self.bonsai_hg_mapping.clone(),
        )
        .and_then({
            cloned!(ctx);
            move |commits_to_generate: Vec<BonsaiChangeset>| {
                let start = (0, commits_to_generate.into_iter());

                loop_fn(
                    start,
                    move |(mut generated_count, mut commits_to_generate)| match commits_to_generate
                        .next()
                    {
                        Some(bcs) => {
                            let bcs_id = bcs.get_changeset_id();

                            generate_single_hg_changeset(ctx.clone(), repo.clone(), bcs)
                                .map({
                                    cloned!(ctx);
                                    move |(hg_cs_id, generated)| {
                                        if generated {
                                            debug!(
                                            ctx.logger(),
                                            "generated hg changeset for {}: {} ({} left to visit)",
                                            bcs_id,
                                            hg_cs_id,
                                            commits_to_generate.len(),
                                        );
                                            generated_count += 1;
                                        }
                                        Loop::Continue((generated_count, commits_to_generate))
                                    }
                                })
                                .left_future()
                        }
                        None => {
                            return bonsai_hg_mapping
                                .get_hg_from_bonsai(ctx.clone(), repoid, bcs_id)
                                .map({
                                    cloned!(ctx);
                                    move |maybe_hg_cs_id| match maybe_hg_cs_id {
                                        Some(hg_cs_id) => {
                                            if generated_count > 0 {
                                                debug!(
                                                    ctx.logger(),
                                                    "generation complete for {}", bcs_id,
                                                );
                                            }
                                            Loop::Break((hg_cs_id, generated_count))
                                        }
                                        None => panic!("hg changeset must be generated already"),
                                    }
                                })
                                .right_future();
                        }
                    },
                )
            }
        })
    }
}

/// This function uploads bonsai changests object to blobstore in parallel, and then does
/// sequential writes to changesets table. Parents of the changesets should already by saved
/// in the repository.
pub fn save_bonsai_changesets(
    bonsai_changesets: Vec<BonsaiChangeset>,
    ctx: CoreContext,
    repo: BlobRepo,
) -> impl Future<Item = (), Error = Error> {
    let complete_changesets = repo.changesets.clone();
    let blobstore = repo.blobstore.clone();
    let repoid = repo.repoid.clone();

    let mut parents_to_check: HashSet<ChangesetId> = HashSet::new();
    for bcs in &bonsai_changesets {
        parents_to_check.extend(bcs.parents());
    }
    // Remove commits that we are uploading in this batch
    for bcs in &bonsai_changesets {
        parents_to_check.remove(&bcs.get_changeset_id());
    }

    let parents_to_check = stream::futures_unordered(parents_to_check.into_iter().map({
        cloned!(ctx, repo);
        move |p| {
            repo.changeset_exists_by_bonsai(ctx.clone(), p)
                .and_then(move |exists| {
                    if exists {
                        Ok(())
                    } else {
                        Err(format_err!("Commit {} does not exist in the repo", p))
                    }
                })
        }
    }))
    .collect();

    let bonsai_changesets: HashMap<_, _> = bonsai_changesets
        .into_iter()
        .map(|bcs| (bcs.get_changeset_id(), bcs))
        .collect();

    // Order of inserting bonsai changesets objects doesn't matter, so we can join them
    let mut bonsai_object_futs = FuturesUnordered::new();
    for bcs in bonsai_changesets.values() {
        bonsai_object_futs.push(save_bonsai_changeset_object(
            ctx.clone(),
            blobstore.clone(),
            bcs.clone(),
        ));
    }
    let bonsai_objects = bonsai_object_futs.collect();
    // Order of inserting entries in changeset table matters though, so we first need to
    // topologically sort commits.
    let mut bcs_parents = HashMap::new();
    for bcs in bonsai_changesets.values() {
        let parents: Vec<_> = bcs.parents().collect();
        bcs_parents.insert(bcs.get_changeset_id(), parents);
    }

    let topo_sorted_commits = sort_topological(&bcs_parents).expect("loop in commit chain!");
    let mut bonsai_complete_futs = vec![];
    for bcs_id in topo_sorted_commits {
        if let Some(bcs) = bonsai_changesets.get(&bcs_id) {
            let bcs_id = bcs.get_changeset_id();
            let completion_record = ChangesetInsert {
                repo_id: repoid,
                cs_id: bcs_id,
                parents: bcs.parents().into_iter().collect(),
            };

            bonsai_complete_futs.push(complete_changesets.add(ctx.clone(), completion_record));
        }
    }

    bonsai_objects
        .join(parents_to_check)
        .and_then(move |_| {
            loop_fn(
                bonsai_complete_futs.into_iter(),
                move |mut futs| match futs.next() {
                    Some(fut) => fut
                        .and_then({ move |_| ok(Loop::Continue(futs)) })
                        .left_future(),
                    None => ok(Loop::Break(())).right_future(),
                },
            )
        })
        .and_then(|_| ok(()))
}

pub struct CreateChangeset {
    /// This should always be provided, keeping it an Option for tests
    pub expected_nodeid: Option<HgNodeHash>,
    pub expected_files: Option<Vec<MPath>>,
    pub p1: Option<ChangesetHandle>,
    pub p2: Option<ChangesetHandle>,
    // root_manifest can be None f.e. when commit removes all the content of the repo
    pub root_manifest: BoxFuture<Option<(HgBlobEntry, RepoPath)>, Error>,
    pub sub_entries: BoxStream<(HgBlobEntry, RepoPath), Error>,
    pub cs_metadata: ChangesetMetadata,
    pub must_check_case_conflicts: bool,
}

impl CreateChangeset {
    pub fn create(
        self,
        ctx: CoreContext,
        repo: &BlobRepo,
        mut scuba_logger: ScubaSampleBuilder,
    ) -> ChangesetHandle {
        STATS::create_changeset.add_value(1);
        // This is used for logging, so that we can tie up all our pieces without knowing about
        // the final commit hash
        let uuid = Uuid::new_v4();
        scuba_logger.add("changeset_uuid", format!("{}", uuid));
        let event_id = EventId::new();

        let entry_processor = UploadEntries::new(repo.blobstore.clone(), scuba_logger.clone());
        let (signal_parent_ready, can_be_parent) = oneshot::channel();
        let signal_parent_ready = Arc::new(Mutex::new(Some(signal_parent_ready)));
        let expected_nodeid = self.expected_nodeid;

        let upload_entries = process_entries(
            ctx.clone(),
            &entry_processor,
            self.root_manifest,
            self.sub_entries,
        )
        .context("While processing entries")
        .traced_with_id(&ctx.trace(), "uploading entries", trace_args!(), event_id);

        let parents_complete = extract_parents_complete(&self.p1, &self.p2);
        let parents_data = handle_parents(scuba_logger.clone(), self.p1, self.p2)
            .context("While waiting for parents to upload data")
            .traced_with_id(
                &ctx.trace(),
                "waiting for parents data",
                trace_args!(),
                event_id,
            );
        let must_check_case_conflicts = self.must_check_case_conflicts.clone();
        let changeset = {
            let mut scuba_logger = scuba_logger.clone();
            upload_entries
                .join(parents_data)
                .from_err()
                .and_then({
                    cloned!(
                        ctx,
                        repo,
                        repo.blobstore,
                        mut scuba_logger,
                        signal_parent_ready
                    );
                    let expected_files = self.expected_files;
                    let cs_metadata = self.cs_metadata;

                    move |(root_mf_id, (parents, parent_manifest_hashes, bonsai_parents))| {
                        let files = if let Some(expected_files) = expected_files {
                            STATS::create_changeset_expected_cf.add_value(1);
                            // We are trusting the callee to provide a list of changed files, used
                            // by the import job
                            future::ok(expected_files).boxify()
                        } else {
                            STATS::create_changeset_compute_cf.add_value(1);
                            compute_changed_files(
                                ctx.clone(),
                                repo.clone(),
                                root_mf_id,
                                parent_manifest_hashes.get(0).cloned(),
                                parent_manifest_hashes.get(1).cloned(),
                            )
                        };

                        let p1_mf = parent_manifest_hashes.get(0).cloned();
                        let check_case_conflicts = if must_check_case_conflicts {
                            cloned!(ctx, repo);
                            async move {
                                check_case_conflicts(&ctx, &repo, root_mf_id, p1_mf).await
                            }
                            .boxed()
                            .compat()
                            .left_future()
                        } else {
                            future::ok(()).right_future()
                        };

                        let changesets = files
                            .join(check_case_conflicts)
                            .and_then(move |(files, ())| {
                                STATS::create_changeset_cf_count.add_value(files.len() as i64);
                                make_new_changeset(parents, root_mf_id, cs_metadata, files)
                            })
                            .and_then({
                                cloned!(ctx, parent_manifest_hashes);
                                move |hg_cs| {
                                    create_bonsai_changeset_object(
                                        ctx,
                                        hg_cs.clone(),
                                        parent_manifest_hashes,
                                        bonsai_parents,
                                        repo.clone(),
                                    )
                                    .map(|bonsai_cs| (hg_cs, bonsai_cs))
                                }
                            });

                        changesets
                            .context("While computing changed files")
                            .and_then({
                                cloned!(ctx);
                                move |(blobcs, bonsai_cs)| {
                                    let fut: BoxFuture<(HgBlobChangeset, BonsaiChangeset), Error> =
                                        (move || {
                                            let bonsai_blob = bonsai_cs.clone().into_blob();
                                            let bcs_id = bonsai_blob.id().clone();

                                            let cs_id = blobcs.get_changeset_id().into_nodehash();
                                            let manifest_id = blobcs.manifestid();

                                            if let Some(expected_nodeid) = expected_nodeid {
                                                if cs_id != expected_nodeid {
                                                    return future::err(
                                                        ErrorKind::InconsistentChangesetHash(
                                                            expected_nodeid,
                                                            cs_id,
                                                            blobcs,
                                                        )
                                                        .into(),
                                                    )
                                                    .boxify();
                                                }
                                            }

                                            scuba_logger
                                                .add("changeset_id", format!("{}", cs_id))
                                                .log_with_msg(
                                                    "Changeset uuid to hash mapping",
                                                    None,
                                                );
                                            // NOTE(luk): an attempt was made in D8187210 to split the
                                            // upload_entries signal into upload_entries and
                                            // processed_entries and to signal_parent_ready after
                                            // upload_entries, so that one doesn't need to wait for the
                                            // entries to be processed. There were no performance gains
                                            // from that experiment
                                            //
                                            // We deliberately eat this error - this is only so that
                                            // another changeset can start verifying data in the blob
                                            // store while we verify this one
                                            let _ = signal_parent_ready
                                                .lock()
                                                .expect("poisoned lock")
                                                .take()
                                                .expect("signal_parent_ready cannot be taken yet")
                                                .send(Ok((bcs_id, cs_id, manifest_id)));

                                            let bonsai_cs_fut = save_bonsai_changeset_object(
                                                ctx.clone(),
                                                blobstore.clone(),
                                                bonsai_cs.clone(),
                                            );

                                            blobcs
                                                .save(ctx.clone(), blobstore)
                                                .join(bonsai_cs_fut)
                                                .context("While writing to blobstore")
                                                .join(
                                                    entry_processor
                                                        .finalize(
                                                            ctx,
                                                            root_mf_id,
                                                            parent_manifest_hashes,
                                                        )
                                                        .context("While finalizing processing"),
                                                )
                                                .from_err()
                                                .map(move |_| (blobcs, bonsai_cs))
                                                .boxify()
                                        })();

                                    fut.context(
                                        "While creating and verifying Changeset for blobstore",
                                    )
                                }
                            })
                            .traced_with_id(
                                &ctx.trace(),
                                "uploading changeset",
                                trace_args!(),
                                event_id,
                            )
                            .from_err()
                    }
                })
                .timed(move |stats, result| {
                    if result.is_ok() {
                        scuba_logger
                            .add_future_stats(&stats)
                            .log_with_msg("Changeset created", None);
                    }
                    Ok(())
                })
                .inspect_err({
                    cloned!(signal_parent_ready);
                    move |e| {
                        let trigger = signal_parent_ready.lock().expect("poisoned lock").take();
                        if let Some(trigger) = trigger {
                            // Ignore errors if the receiving end has gone away.
                            let e = format_err!("signal_parent_ready failed: {:?}", e);
                            let _ = trigger.send(Err(e));
                        }
                    }
                })
        };

        let parents_complete = parents_complete
            .context("While waiting for parents to complete")
            .traced_with_id(
                &ctx.trace(),
                "waiting for parents complete",
                trace_args!(),
                event_id,
            )
            .timed({
                let mut scuba_logger = scuba_logger.clone();
                move |stats, result| {
                    if result.is_ok() {
                        scuba_logger
                            .add_future_stats(&stats)
                            .log_with_msg("Parents completed", None);
                    }
                    Ok(())
                }
            });

        let complete_changesets = repo.changesets.clone();
        cloned!(repo, repo.repoid);
        let changeset_complete_fut = changeset
            .join(parents_complete)
            .and_then({
                cloned!(ctx, repo.bonsai_hg_mapping);
                move |((hg_cs, bonsai_cs), _)| {
                    let bcs_id = bonsai_cs.get_changeset_id();
                    let bonsai_hg_entry = BonsaiHgMappingEntry {
                        repo_id: repoid.clone(),
                        hg_cs_id: hg_cs.get_changeset_id(),
                        bcs_id,
                    };

                    bonsai_hg_mapping
                        .add(ctx.clone(), bonsai_hg_entry)
                        .map(move |_| (hg_cs, bonsai_cs))
                        .context("While inserting mapping")
                        .traced_with_id(
                            &ctx.trace(),
                            "uploading bonsai hg mapping",
                            trace_args!(),
                            event_id,
                        )
                }
            })
            .and_then(move |(hg_cs, bonsai_cs)| {
                let completion_record = ChangesetInsert {
                    repo_id: repo.repoid,
                    cs_id: bonsai_cs.get_changeset_id(),
                    parents: bonsai_cs.parents().into_iter().collect(),
                };
                complete_changesets
                    .add(ctx.clone(), completion_record)
                    .map(|_| (bonsai_cs, hg_cs))
                    .context("While inserting into changeset table")
                    .traced_with_id(
                        &ctx.trace(),
                        "uploading final changeset",
                        trace_args!(),
                        event_id,
                    )
            })
            .with_context(move || {
                format!(
                    "While creating Changeset {:?}, uuid: {}",
                    expected_nodeid, uuid
                )
            })
            .timed({
                move |stats, result| {
                    if result.is_ok() {
                        scuba_logger
                            .add_future_stats(&stats)
                            .log_with_msg("CreateChangeset Finished", None);
                    }
                    Ok(())
                }
            });

        let can_be_parent = can_be_parent
            .into_future()
            .then(|r| match r {
                Ok(res) => res,
                Err(e) => Err(format_err!("can_be_parent: {:?}", e)),
            })
            .map_err(Compat)
            .boxify()
            .shared();

        ChangesetHandle::new_pending(
            can_be_parent,
            spawn_future(changeset_complete_fut)
                .map_err(Compat)
                .boxify()
                .shared(),
        )
    }
}

impl Clone for BlobRepo {
    fn clone(&self) -> Self {
        Self {
            bookmarks: self.bookmarks.clone(),
            blobstore: self.blobstore.clone(),
            filenodes: self.filenodes.clone(),
            changesets: self.changesets.clone(),
            bonsai_git_mapping: self.bonsai_git_mapping.clone(),
            bonsai_globalrev_mapping: self.bonsai_globalrev_mapping.clone(),
            bonsai_hg_mapping: self.bonsai_hg_mapping.clone(),
            hg_mutation_store: self.hg_mutation_store.clone(),
            repoid: self.repoid.clone(),
            changeset_fetcher_factory: self.changeset_fetcher_factory.clone(),
            derived_data_lease: self.derived_data_lease.clone(),
            filestore_config: self.filestore_config.clone(),
            phases_factory: self.phases_factory.clone(),
            derived_data_config: self.derived_data_config.clone(),
            reponame: self.reponame.clone(),
        }
    }
}

fn to_hg_bookmark_stream<T>(
    repo: &BlobRepo,
    ctx: &CoreContext,
    stream: T,
) -> impl Stream<Item = (Bookmark, HgChangesetId), Error = Error>
where
    T: Stream<Item = (Bookmark, ChangesetId), Error = Error>,
{
    stream
        .chunks(100)
        .map({
            cloned!(repo, ctx);
            move |chunk| {
                let cs_ids = chunk.iter().map(|(_, cs_id)| *cs_id).collect::<Vec<_>>();

                repo.get_hg_bonsai_mapping(ctx.clone(), cs_ids)
                    .map(move |mapping| {
                        let mapping = mapping
                            .into_iter()
                            .map(|(hg_cs_id, cs_id)| (cs_id, hg_cs_id))
                            .collect::<HashMap<_, _>>();

                        let res = chunk
                            .into_iter()
                            .map(|(bookmark, cs_id)| {
                                let hg_cs_id = mapping.get(&cs_id).ok_or_else(|| {
                                    anyhow::format_err!(
                                        "cs_id was missing from mapping: {:?}",
                                        cs_id
                                    )
                                })?;
                                Ok((bookmark, *hg_cs_id))
                            })
                            .collect::<Vec<_>>();

                        stream::iter_result(res)
                    })
                    .flatten_stream()
            }
        })
        .flatten()
}

impl DangerousOverride<Arc<dyn LeaseOps>> for BlobRepo {
    fn dangerous_override<F>(&self, modify: F) -> Self
    where
        F: FnOnce(Arc<dyn LeaseOps>) -> Arc<dyn LeaseOps>,
    {
        let derived_data_lease = modify(self.derived_data_lease.clone());
        BlobRepo {
            derived_data_lease,
            ..self.clone()
        }
    }
}

impl DangerousOverride<Arc<dyn Blobstore>> for BlobRepo {
    fn dangerous_override<F>(&self, modify: F) -> Self
    where
        F: FnOnce(Arc<dyn Blobstore>) -> Arc<dyn Blobstore>,
    {
        let (blobstore, repoid) = RepoBlobstoreArgs::new_with_wrapped_inner_blobstore(
            self.blobstore.clone(),
            self.get_repoid(),
            modify,
        )
        .into_blobrepo_parts();
        BlobRepo {
            repoid,
            blobstore,
            ..self.clone()
        }
    }
}

impl DangerousOverride<Arc<dyn Bookmarks>> for BlobRepo {
    fn dangerous_override<F>(&self, modify: F) -> Self
    where
        F: FnOnce(Arc<dyn Bookmarks>) -> Arc<dyn Bookmarks>,
    {
        let bookmarks = modify(self.bookmarks.clone());
        BlobRepo {
            bookmarks,
            ..self.clone()
        }
    }
}

impl DangerousOverride<Arc<dyn Filenodes>> for BlobRepo {
    fn dangerous_override<F>(&self, modify: F) -> Self
    where
        F: FnOnce(Arc<dyn Filenodes>) -> Arc<dyn Filenodes>,
    {
        let filenodes = modify(self.filenodes.clone());
        BlobRepo {
            filenodes,
            ..self.clone()
        }
    }
}

impl DangerousOverride<Arc<dyn Changesets>> for BlobRepo {
    fn dangerous_override<F>(&self, modify: F) -> Self
    where
        F: FnOnce(Arc<dyn Changesets>) -> Arc<dyn Changesets>,
    {
        let changesets = modify(self.changesets.clone());

        let changeset_fetcher_factory = {
            cloned!(changesets, self.repoid);
            move || {
                let res: Arc<dyn ChangesetFetcher + Send + Sync> = Arc::new(
                    SimpleChangesetFetcher::new(changesets.clone(), repoid.clone()),
                );
                res
            }
        };

        BlobRepo {
            changesets,
            changeset_fetcher_factory: Arc::new(changeset_fetcher_factory),
            ..self.clone()
        }
    }
}

impl DangerousOverride<Arc<dyn BonsaiHgMapping>> for BlobRepo {
    fn dangerous_override<F>(&self, modify: F) -> Self
    where
        F: FnOnce(Arc<dyn BonsaiHgMapping>) -> Arc<dyn BonsaiHgMapping>,
    {
        let bonsai_hg_mapping = modify(self.bonsai_hg_mapping.clone());
        BlobRepo {
            bonsai_hg_mapping,
            ..self.clone()
        }
    }
}

impl DangerousOverride<DerivedDataConfig> for BlobRepo {
    fn dangerous_override<F>(&self, modify: F) -> Self
    where
        F: FnOnce(DerivedDataConfig) -> DerivedDataConfig,
    {
        let derived_data_config = modify(self.derived_data_config.clone());
        BlobRepo {
            derived_data_config,
            ..self.clone()
        }
    }
}

impl DangerousOverride<FilestoreConfig> for BlobRepo {
    fn dangerous_override<F>(&self, modify: F) -> Self
    where
        F: FnOnce(FilestoreConfig) -> FilestoreConfig,
    {
        let filestore_config = modify(self.filestore_config.clone());
        BlobRepo {
            filestore_config,
            ..self.clone()
        }
    }
}
