/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::convert::TryFrom;

use anyhow::anyhow;
use bytes::Bytes;
use futures::{
    stream::{select_all, BoxStream},
    StreamExt, TryStreamExt,
};
use gotham::state::{FromState, State};
use gotham_derive::{StateData, StaticResponseExtender};
use serde::Deserialize;

use gotham_ext::{error::HttpError, response::BytesBody};
use mercurial_types::{HgFileNodeId, HgNodeHash};
use mononoke_api::hg::HgRepoContext;
use mononoke_types::MPath;
use types::{
    api::{HistoryRequest, HistoryResponse},
    Key, WireHistoryEntry,
};

use crate::context::ServerContext;
use crate::middleware::RequestContext;

use super::util::{cbor_mime, get_repo, get_request_body};

type HistoryStream = BoxStream<'static, Result<WireHistoryEntry, HttpError>>;

#[derive(Debug, Deserialize, StateData, StaticResponseExtender)]
pub struct HistoryParams {
    repo: String,
}

pub async fn history(state: &mut State) -> Result<BytesBody<Bytes>, HttpError> {
    let rctx = RequestContext::borrow_from(state);
    let sctx = ServerContext::borrow_from(state);
    let params = HistoryParams::borrow_from(state);

    let repo = get_repo(&sctx, &rctx, &params.repo).await?;
    let body = get_request_body(state).await?;

    let request = serde_cbor::from_slice(&body).map_err(HttpError::e400)?;
    let response = get_history(&repo, request).await?;
    let bytes: Bytes = serde_cbor::to_vec(&response)
        .map_err(HttpError::e500)?
        .into();

    Ok(BytesBody::new(bytes, cbor_mime()))
}

/// Fetch data for all of the requested keys concurrently.
async fn get_history(
    repo: &HgRepoContext,
    request: HistoryRequest,
) -> Result<HistoryResponse, HttpError> {
    // Get streams of history entries for all requested keys.
    let mut streams = Vec::with_capacity(request.keys.len());
    for key in request.keys {
        // NOTE: request.depth is a confusing name - it's not the depth
        // but rather a maximum length of the returned history.
        let entries = single_key_history(repo, &key, request.depth).await?;
        // Add the path of the current key to all items of the stream.
        // This is needed since the history entries of different keys
        // may be arbitrarily interleaved later.
        let entries = entries.map_ok(move |entry| (key.path.clone(), entry));
        streams.push(entries);
    }

    // Combine them into a single stream, then buffer all items.
    // TODO(kulshrax): Don't buffer the results here.
    let entries = select_all(streams).try_collect().await?;
    let response = HistoryResponse { entries };

    Ok(response)
}

async fn single_key_history(
    repo: &HgRepoContext,
    key: &Key,
    length: Option<u32>,
) -> Result<HistoryStream, HttpError> {
    let filenode_id = HgFileNodeId::new(HgNodeHash::from(key.hgid));
    let path = MPath::new(key.path.as_byte_slice()).map_err(HttpError::e400)?;

    let file = repo
        .file(filenode_id)
        .await
        .map_err(HttpError::e500)?
        .ok_or_else(|| HttpError::e404(anyhow!("file not found: {:?}", &key)))?;

    // Fetch the file's history and convert the entries into
    // the expected on-the-wire format.
    let history = file
        .history(path, length)
        .map_err(HttpError::e500)
        // XXX: Use async block because TryStreamExt::and_then
        // requires the closure to return a TryFuture.
        .and_then(|entry| async { WireHistoryEntry::try_from(entry).map_err(HttpError::e500) })
        .boxed();

    Ok(history)
}
