//! Video jobs (after OpenCode's `VideoGeneration`): a video is submitted
//! to its provider, polled until it finishes, and its files are kept in the
//! session database's blob store (see [`crate::blob`]), at most 20 MiB each.
//!
//! Each job is a row of `video_job`, written once the provider accepted it,
//! so another process (or this one after a restart) picks the polling up
//! where it was. A job gives up two hours after submission; a finished
//! job's row, and the blobs only it holds, are dropped a day after it
//! finished.

use crate::config::Config;
use crate::generate::{self, GenerateError, MediaModel};
use crate::store::Store;
use anyhow::Result;
use codesmith_ai::generation::{self as gen, VideoState};
use codesmith_ai::Media;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How often a running job is checked.
pub const INTERVAL: Duration = Duration::from_secs(5);
/// How long after submission a job gives up.
pub const DEADLINE_MS: i64 = 2 * 60 * 60 * 1000;
/// How long a finished job is kept.
pub const RETENTION_MS: i64 = 24 * 60 * 60 * 1000;

const TABLE: &str = "CREATE TABLE IF NOT EXISTS video_job (
    id TEXT PRIMARY KEY, body TEXT NOT NULL, updated INTEGER NOT NULL);";

/// One stored video of a finished job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Blob {
    /// Its blob hash.
    pub id: String,
    pub mime: String,
    pub bytes: u64,
}

/// A job's stored state. `remote` is the provider's id for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Record {
    Running {
        remote: String,
        model: String,
        created: i64,
    },
    Completed {
        model: String,
        created: i64,
        blobs: Vec<Blob>,
        finished: i64,
    },
    Error {
        model: String,
        created: i64,
        /// `failed`, `expired`, `cancelled` or `unknown`.
        reason: String,
        error: String,
        finished: i64,
    },
}

impl Record {
    fn finished_at(&self) -> i64 {
        match self {
            Record::Running { created, .. } => created + DEADLINE_MS,
            Record::Completed { finished, .. } | Record::Error { finished, .. } => *finished,
        }
    }

    fn model(&self) -> &str {
        match self {
            Record::Running { model, .. }
            | Record::Completed { model, .. }
            | Record::Error { model, .. } => model,
        }
    }

    fn created(&self) -> i64 {
        match self {
            Record::Running { created, .. }
            | Record::Completed { created, .. }
            | Record::Error { created, .. } => *created,
        }
    }
}

/// What a reader learns about a job.
#[derive(Debug, Clone, PartialEq)]
pub enum Job {
    Running,
    Completed { blobs: Vec<Blob> },
    Error { reason: String, error: String },
}

/// A video request: the model as `provider/model`.
#[derive(Debug, Clone, Default)]
pub struct Input {
    pub model: String,
    pub prompt: String,
    pub start: Option<Media>,
    pub end: Option<Media>,
    pub references: Vec<Media>,
    pub duration: Option<u32>,
    pub aspect: Option<String>,
    pub resolution: Option<String>,
    pub sound: Option<bool>,
}

/// The blob owner a job's videos are kept under.
pub fn owner(id: &str) -> String {
    format!("video:{id}")
}

/// A new job id.
pub fn new_id() -> String {
    format!("video_{}", crate::tool::image_generate::job_suffix())
}

/// Video jobs in one session database.
pub struct Videos<'a> {
    pub store: &'a Store,
    pub config: &'a Config,
    pub client: &'a reqwest::Client,
}

fn model_for(config: &Config, spec: &str) -> Result<(MediaModel, gen::VideoApi), GenerateError> {
    let (m, _) = generate::resolve(config, "video", spec)?;
    let api = m
        .video_api()
        .ok_or_else(|| GenerateError::UnsupportedModel {
            provider: m.provider_id.clone(),
            model: m.id.clone(),
            capability: "video".into(),
            message: format!("{} has no video API CodeSmith can call", m.spec()),
        })?;
    Ok((m, api))
}

impl Videos<'_> {
    fn load(&self, id: &str) -> Result<Option<Record>> {
        self.store.with_conn(|c| {
            c.execute_batch(TABLE)?;
            let body: Option<String> = c
                .query_row("SELECT body FROM video_job WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()?;
            Ok(body.and_then(|b| serde_json::from_str(&b).ok()))
        })
    }

    fn save(&self, id: &str, r: &Record) -> Result<()> {
        let body = serde_json::to_string(r)?;
        self.store.with_conn(|c| {
            c.execute_batch(TABLE)?;
            c.execute(
                "INSERT INTO video_job (id, body, updated) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET body = ?2, updated = ?3",
                params![id, body, crate::store::now()],
            )?;
            Ok(())
        })
    }

    /// Write `outcome` only while the job still runs: one cancelled
    /// meanwhile keeps its row, and what was downloaded is let go.
    fn finish(&self, id: &str, outcome: Record) -> Result<bool> {
        let body = serde_json::to_string(&outcome)?;
        let updated = self.store.with_conn(|c| {
            c.execute_batch(TABLE)?;
            Ok(c.execute(
                "UPDATE video_job SET body = ?2, updated = ?3
                 WHERE id = ?1 AND json_extract(body, '$.status') = 'running'",
                params![id, body, crate::store::now()],
            )? > 0)
        })?;
        if !updated && matches!(outcome, Record::Completed { .. }) {
            self.store.release_blobs(&owner(id))?;
        }
        Ok(updated)
    }

    /// Submit a video; returns once the provider accepted it. Submitting
    /// is paid and not idempotent, so it is never retried.
    pub async fn start(&self, input: &Input) -> Result<String, GenerateError> {
        let (m, api) = model_for(self.config, &input.model)?;
        // Each new job clears the rows of jobs that finished a day ago.
        let _ = self.sweep_at(crate::store::now());
        let req = gen::VideoRequest {
            model: m.model_id.clone(),
            prompt: input.prompt.clone(),
            start: input.start.clone(),
            end: input.end.clone(),
            references: input.references.clone(),
            duration: input.duration,
            aspect: input.aspect.clone(),
            resolution: input.resolution.clone(),
            sound: input.sound,
            options: Default::default(),
        };
        let remote = gen::video_start(&m.provider, self.client, api, &req)
            .await
            .map_err(|e| generate::provider_error(&m.provider_id, &e))?;
        let id = new_id();
        self.save(
            &id,
            &Record::Running {
                remote,
                model: m.spec(),
                created: crate::store::now(),
            },
        )
        .map_err(|e| GenerateError::Unavailable {
            message: format!("{e:#}"),
        })?;
        Ok(id)
    }

    /// The job, or `None` when there is none (or it expired).
    pub fn get(&self, id: &str) -> Result<Option<Job>> {
        self.get_at(id, crate::store::now())
    }

    pub fn get_at(&self, id: &str, now: i64) -> Result<Option<Job>> {
        let Some(r) = self.load(id)? else {
            return Ok(None);
        };
        if r.finished_at() + RETENTION_MS <= now {
            return Ok(None);
        }
        Ok(Some(match r {
            Record::Running { .. } => Job::Running,
            Record::Error { reason, error, .. } => Job::Error { reason, error },
            Record::Completed { blobs, .. } => {
                // A deleted blob is dropped.
                let kept: Vec<Blob> = blobs
                    .into_iter()
                    .filter(|b| self.store.blob(&b.id).ok().flatten().is_some())
                    .collect();
                if kept.is_empty() {
                    return Ok(None);
                }
                Job::Completed { blobs: kept }
            }
        }))
    }

    /// One check of a running job: give up past the deadline, read the
    /// status (a failed read is tried at the next check), and on success
    /// download every video, all or nothing.
    pub async fn check(&self, id: &str) -> Result<()> {
        let Some(r @ Record::Running { .. }) = self.load(id)? else {
            return Ok(());
        };
        let Record::Running {
            remote,
            model,
            created,
        } = r.clone()
        else {
            unreachable!()
        };
        let now = crate::store::now();
        let error = |reason: &str, error: String| Record::Error {
            model: model.clone(),
            created,
            reason: reason.into(),
            error,
            finished: crate::store::now(),
        };
        if now >= created + DEADLINE_MS {
            self.finish(
                id,
                error(
                    "unknown",
                    "The provider had no result 2 hours after submission".into(),
                ),
            )?;
            return Ok(());
        }
        let (m, api) = match model_for(self.config, &model) {
            Ok(x) => x,
            Err(e) => {
                self.finish(id, error("failed", e.to_string()))?;
                return Ok(());
            }
        };
        let state = match gen::video_status(&m.provider, self.client, api, &remote).await {
            Ok(s) => s,
            Err(_) => return Ok(()),
        };
        match state {
            VideoState::Running { .. } => Ok(()),
            VideoState::Failed { reason, .. } => {
                self.finish(
                    id,
                    error(
                        &reason,
                        format!("The provider reported the job as {reason}"),
                    ),
                )?;
                Ok(())
            }
            VideoState::Completed => {
                let mut tries = 0;
                let outcome = loop {
                    match self.download(id, &m, api, &remote).await {
                        Ok(blobs) => {
                            break Record::Completed {
                                model: model.clone(),
                                created,
                                blobs,
                                finished: crate::store::now(),
                            }
                        }
                        Err(e) if tries < 2 => {
                            let _ = e;
                            tries += 1;
                            tokio::time::sleep(Duration::from_secs(1 << tries)).await;
                        }
                        Err(e) => break error("failed", e.to_string()),
                    }
                };
                self.finish(id, outcome)?;
                Ok(())
            }
        }
    }

    async fn download(
        &self,
        id: &str,
        m: &MediaModel,
        api: gen::VideoApi,
        remote: &str,
    ) -> Result<Vec<Blob>, GenerateError> {
        let fail = |e: &codesmith_ai::Error| generate::provider_error(&m.provider_id, e);
        let outputs = gen::video_result(&m.provider, self.client, api, remote)
            .await
            .map_err(|e| fail(&e))?;
        let owner = owner(id);
        let mut blobs = Vec::new();
        let result: Result<(), GenerateError> = async {
            for out in outputs {
                let (mime, data) = generate::materialize(&m.provider, self.client, out)
                    .await
                    .map_err(|e| fail(&e))?;
                if data.is_empty() {
                    return Err(GenerateError::ProviderRequest {
                        provider: m.provider_id.clone(),
                        reason: generate::Failure::Provider,
                        message: "The provider returned an empty video".into(),
                        status: None,
                    });
                }
                let hash = self
                    .store
                    .put_blob(&owner, &data, &mime)
                    .map_err(|e| GenerateError::Unavailable {
                        message: format!("Could not store the video: {e:#}"),
                    })?
                    .ok_or_else(|| GenerateError::ProviderRequest {
                        provider: m.provider_id.clone(),
                        reason: generate::Failure::Provider,
                        message: "The video is larger than 20 MiB".into(),
                        status: None,
                    })?;
                blobs.push(Blob {
                    id: hash,
                    mime,
                    bytes: data.len() as u64,
                });
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            // All or nothing: a job never reports only some of its videos.
            let _ = self.store.release_blobs(&owner);
            return Err(e);
        }
        Ok(blobs)
    }

    /// Wait until the job completes or fails, checking it every `interval`
    /// (whichever process polls it, the row tells).
    pub async fn wait(&self, id: &str, interval: Duration) -> Result<Option<Job>> {
        loop {
            match self.get(id)? {
                Some(Job::Running) => {}
                other => return Ok(other),
            }
            self.check(id).await?;
            match self.get(id)? {
                Some(Job::Running) => tokio::time::sleep(interval).await,
                other => return Ok(other),
            }
        }
    }

    /// Fail a running job as cancelled, and ask the provider to stop it if
    /// it can. `false` when there is no such job.
    pub async fn cancel(&self, id: &str) -> Result<bool> {
        let Some(r) = self.load(id)? else {
            return Ok(false);
        };
        let Record::Running { remote, model, .. } = &r else {
            return Ok(true);
        };
        self.finish(
            id,
            Record::Error {
                model: model.clone(),
                created: r.created(),
                reason: "cancelled".into(),
                error: "The video generation was cancelled".into(),
                finished: crate::store::now(),
            },
        )?;
        // Best effort: some providers can't cancel, and the paid job then
        // runs to completion on their side.
        if let Ok((m, api)) = model_for(self.config, r.model()) {
            let _ = gen::video_cancel(&m.provider, self.client, api, remote).await;
        }
        Ok(true)
    }

    /// Drop the rows (and the blobs only they hold) of jobs that finished
    /// more than a day before `now`, and rows that no longer read.
    pub fn sweep_at(&self, now: i64) -> Result<usize> {
        let rows: Vec<(String, String)> = self.store.with_conn(|c| {
            c.execute_batch(TABLE)?;
            let mut st = c.prepare("SELECT id, body FROM video_job")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        let mut dropped = 0;
        for (id, body) in rows {
            let expired = match serde_json::from_str::<Record>(&body) {
                Ok(r) => r.finished_at() + RETENTION_MS <= now,
                Err(_) => true,
            };
            if expired {
                self.store.with_conn(|c| {
                    c.execute("DELETE FROM video_job WHERE id = ?1", [&id])?;
                    Ok(())
                })?;
                self.store.release_blobs(&owner(&id))?;
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    /// The ids of the jobs still running, to pick their polling up.
    pub fn running(&self) -> Result<Vec<String>> {
        let rows: Vec<(String, String)> = self.store.with_conn(|c| {
            c.execute_batch(TABLE)?;
            let mut st = c.prepare("SELECT id, body FROM video_job")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        Ok(rows
            .into_iter()
            .filter(|(_, b)| {
                matches!(
                    serde_json::from_str::<Record>(b),
                    Ok(Record::Running { .. })
                )
            })
            .map(|(id, _)| id)
            .collect())
    }
}
