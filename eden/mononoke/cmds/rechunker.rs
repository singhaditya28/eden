/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

#![deny(warnings)]

use anyhow::{format_err, Error};
use blobstore::Loadable;
use clap::Arg;
use cloned::cloned;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::future::Future;
use futures::stream;
use futures::stream::Stream;
use futures_ext::FutureExt;
use futures_preview::compat::Future01CompatExt;
use mercurial_types::{HgFileNodeId, HgNodeHash};
use std::str::FromStr;

use cmdlib::{args, helpers::block_execute};

const NAME: &str = "rechunker";
const DEFAULT_NUM_JOBS: usize = 10;

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let matches = args::MononokeApp::new(NAME)
        .with_advanced_args_hidden()
        .build()
        .version("0.0.0")
        .about("Rechunk blobs using the filestore")
        .arg(
            Arg::with_name("filenodes")
                .value_name("FILENODES")
                .takes_value(true)
                .required(true)
                .min_values(1)
                .help("filenode IDs for blobs to be rechunked"),
        )
        .arg(
            Arg::with_name("jobs")
                .short("j")
                .long("jobs")
                .value_name("JOBS")
                .takes_value(true)
                .help("The number of filenodes to rechunk in parallel"),
        )
        .get_matches();

    args::init_cachelib(fb, &matches, None);

    let logger = args::init_logging(fb, &matches);
    let ctx = CoreContext::new_with_logger(fb, logger.clone());
    let blobrepo = args::open_repo(fb, &logger, &matches);

    let jobs: usize = matches
        .value_of("jobs")
        .map_or(Ok(DEFAULT_NUM_JOBS), |j| j.parse())
        .map_err(Error::from)?;

    let filenode_ids: Vec<_> = matches
        .values_of("filenodes")
        .unwrap()
        .into_iter()
        .map(|f| {
            HgNodeHash::from_str(f)
                .map(HgFileNodeId::new)
                .map_err(|e| format_err!("Invalid Sha1: {}", e))
        })
        .collect();

    let rechunk = blobrepo
        .and_then(move |blobrepo| {
            stream::iter_result(filenode_ids)
                .map({
                    cloned!(blobrepo);
                    move |fid| {
                        fid.load(ctx.clone(), blobrepo.blobstore())
                            .from_err()
                            .map(|env| env.content_id())
                            .and_then({
                                cloned!(blobrepo, ctx);
                                move |content_id| {
                                    filestore::rechunk(
                                        blobrepo.get_blobstore(),
                                        blobrepo.filestore_config().clone(),
                                        ctx,
                                        content_id,
                                    )
                                }
                            })
                    }
                })
                .buffered(jobs)
                .for_each(|_| Ok(()))
        })
        .boxify();

    block_execute(
        rechunk.compat(),
        fb,
        "rechunker",
        &logger,
        &matches,
        cmdlib::monitoring::AliveService,
    )
}