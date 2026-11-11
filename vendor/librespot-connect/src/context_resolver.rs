use crate::{
    core::{Error, Session},
    protocol::{
        autoplay_context_request::AutoplayContextRequest, context::Context,
        transfer_state::TransferState,
    },
    state::{ConnectState, context::ContextType},
};
use std::{
    cmp::PartialEq,
    collections::{HashMap, VecDeque},
    fmt::{Display, Formatter},
    hash::Hash,
    time::Duration,
};
use thiserror::Error as ThisError;
use tokio::time::Instant;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum Resolve {
    Uri(String),
    Context(Context),
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(super) enum ContextAction {
    Append,
    Replace,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(super) struct ResolveContext {
    resolve: Resolve,
    fallback: Option<String>,
    update: ContextType,
    action: ContextAction,
}

impl ResolveContext {
    fn append_context(uri: impl Into<String>) -> Self {
        Self {
            resolve: Resolve::Uri(uri.into()),
            fallback: None,
            update: ContextType::Default,
            action: ContextAction::Append,
        }
    }

    pub fn from_uri(
        uri: impl Into<String>,
        fallback: impl Into<String>,
        update: ContextType,
        action: ContextAction,
    ) -> Self {
        let fallback_uri = fallback.into();
        Self {
            resolve: Resolve::Uri(uri.into()),
            fallback: (!fallback_uri.is_empty()).then_some(fallback_uri),
            update,
            action,
        }
    }

    pub fn from_context(context: Context, update: ContextType, action: ContextAction) -> Self {
        Self {
            resolve: Resolve::Context(context),
            fallback: None,
            update,
            action,
        }
    }

    /// the uri which should be used to resolve the context, might not be the context uri
    fn resolve_uri(&self) -> Option<&str> {
        // it's important to call this always, or at least for every ResolveContext
        // otherwise we might not even check if we need to fallback and just use the fallback uri
        match self.resolve {
            Resolve::Uri(ref uri) => ConnectState::valid_resolve_uri(uri),
            Resolve::Context(ref ctx) => {
                ConnectState::find_valid_uri(ctx.uri.as_deref(), ctx.pages.first())
            }
        }
        .or(self.fallback.as_deref())
    }

    /// the actual context uri
    fn context_uri(&self) -> &str {
        match self.resolve {
            Resolve::Uri(ref uri) => uri,
            Resolve::Context(ref ctx) => ctx.uri.as_deref().unwrap_or_default(),
        }
    }
}

impl Display for ResolveContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "resolve_uri: <{:?}>, context_uri: <{}>, update: <{:?}>",
            self.resolve_uri(),
            self.context_uri(),
            self.update,
        )
    }
}

#[derive(Debug, ThisError)]
enum ContextResolverError {
    #[error("no next context to resolve")]
    NoNext,
    #[error("tried appending context with {0} pages")]
    UnexpectedPagesSize(usize),
    #[error("tried resolving not allowed context: {0:?}")]
    NotAllowedContext(String),
}

impl From<ContextResolverError> for Error {
    fn from(value: ContextResolverError) -> Self {
        Error::failed_precondition(value)
    }
}

pub struct ContextResolver {
    session: Session,
    queue: VecDeque<ResolveContext>,
    unavailable_contexts: HashMap<ResolveContext, Instant>,
    /// Cadence patch: autoplay failures in a row, and when autoplay may be asked again.
    autoplay_failures: u32,
    autoplay_retry_at: Option<Instant>,
}

// time after which an unavailable context is retried
const RETRY_UNAVAILABLE: Duration = Duration::from_secs(3600);

/// Cadence patch: first autoplay retry delay, doubled per failure up to the cap.
const AUTOPLAY_BACKOFF_BASE: Duration = Duration::from_secs(5);
const AUTOPLAY_BACKOFF_MAX: Duration = Duration::from_secs(300);

/// Cadence patch: wait after `failures` failed autoplay requests in a row (1 = first failure).
pub(crate) fn autoplay_backoff(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(16);
    AUTOPLAY_BACKOFF_BASE
        .saturating_mul(1 << shift)
        .min(AUTOPLAY_BACKOFF_MAX)
}

impl ContextResolver {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            queue: VecDeque::new(),
            unavailable_contexts: HashMap::new(),
            autoplay_failures: 0,
            autoplay_retry_at: None,
        }
    }

    /// Cadence patch: the next resolve to run is an autoplay one.
    pub fn next_is_autoplay(&self) -> bool {
        matches!(self.find_next(), Some((next, _, _)) if next.update == ContextType::Autoplay)
    }

    /// Cadence patch: backs autoplay off exponentially (capped) after a failure.
    pub fn autoplay_failed(&mut self, why: &str) {
        self.autoplay_failures = self.autoplay_failures.saturating_add(1);
        let wait = autoplay_backoff(self.autoplay_failures);
        self.autoplay_retry_at = Some(Instant::now() + wait);
        crate::autoplay_log(&format!(
            "autoplay: failed ({why}); attempt {}, next try in {}s at the earliest",
            self.autoplay_failures,
            wait.as_secs()
        ));
    }

    pub fn autoplay_succeeded(&mut self) {
        self.autoplay_failures = 0;
        self.autoplay_retry_at = None;
    }

    pub fn add(&mut self, resolve: ResolveContext) {
        // Cadence patch: autoplay retries follow the backoff instead of the unavailable list.
        if resolve.update == ContextType::Autoplay {
            if let Some(at) = self.autoplay_retry_at {
                let now = Instant::now();
                if now < at {
                    crate::autoplay_log(&format!(
                        "autoplay: skipped, backing off for {}s more",
                        (at - now).as_secs()
                    ));
                    return;
                }
            }
            if self.queue.contains(&resolve) {
                return;
            }
            crate::autoplay_log(&format!(
                "autoplay: requested for seed {} ({:?})",
                resolve.resolve_uri().unwrap_or("?"),
                resolve.action
            ));
            self.queue.push_back(resolve);
            return;
        }

        // Cadence patch: 0.8.0 measured `duration_since(now)` (always 0), so an unavailable
        // context was never retried and the map never shrank.
        self.unavailable_contexts
            .retain(|_, at| at.elapsed() < RETRY_UNAVAILABLE);
        let last_try = self.unavailable_contexts.get(&resolve).map(|i| i.elapsed());

        if last_try.is_some() {
            debug!("tried loading unavailable context: {resolve}");
            return;
        } else if self.queue.contains(&resolve) {
            debug!("update for {resolve} is already added");
            return;
        } else {
            trace!(
                "added {} to resolver queue",
                resolve.resolve_uri().unwrap_or(resolve.context_uri())
            )
        }

        self.queue.push_back(resolve)
    }

    pub fn add_list(&mut self, resolve: Vec<ResolveContext>) {
        for resolve in resolve {
            self.add(resolve)
        }
    }

    pub fn remove_used_and_invalid(&mut self) {
        if let Some((_, _, remove)) = self.find_next() {
            let _ = self.queue.drain(0..remove); // remove invalid
        }
        self.queue.pop_front(); // remove used
    }

    /// Drops pending resolves (a new load). Cadence patch: the new context gets a fresh
    /// autoplay try.
    pub fn clear(&mut self) {
        self.queue = VecDeque::new();
        self.autoplay_succeeded();
    }

    fn find_next(&self) -> Option<(&ResolveContext, &str, usize)> {
        for idx in 0..self.queue.len() {
            let next = self.queue.get(idx)?;
            match next.resolve_uri() {
                None => {
                    warn!("skipped {idx} because of invalid resolve_uri: {next}");
                    continue;
                }
                Some(uri) => return Some((next, uri, idx)),
            }
        }
        None
    }

    pub fn has_next(&self) -> bool {
        self.find_next().is_some()
    }

    pub async fn get_next_context(
        &self,
        recent_track_uri: impl Fn() -> Vec<String>,
    ) -> Result<Context, Error> {
        let (next, resolve_uri, _) = self.find_next().ok_or(ContextResolverError::NoNext)?;

        match next.update {
            ContextType::Default => {
                let mut ctx = self.session.spclient().get_context(resolve_uri).await;
                if let Ok(ctx) = ctx.as_mut() {
                    ctx.uri = Some(next.context_uri().to_string());
                    ctx.url = ctx.uri.as_ref().map(|s| format!("context://{s}"));
                }

                ctx
            }
            ContextType::Autoplay => {
                if resolve_uri.contains("spotify:show:") || resolve_uri.contains("spotify:episode:")
                {
                    // autoplay is not supported for podcasts
                    Err(ContextResolverError::NotAllowedContext(
                        resolve_uri.to_string(),
                    ))?
                }

                let request = AutoplayContextRequest {
                    context_uri: Some(resolve_uri.to_string()),
                    recent_track_uri: recent_track_uri(),
                    ..Default::default()
                };
                self.session.spclient().get_autoplay_context(&request).await
            }
        }
    }

    pub fn mark_next_unavailable(&mut self) {
        if let Some((next, _, _)) = self.find_next() {
            // Cadence patch: autoplay uses the backoff (handle_next_context calls autoplay_failed)
            if next.update == ContextType::Autoplay {
                return;
            }
            self.unavailable_contexts
                .insert(next.clone(), Instant::now());
        }
    }

    pub fn apply_next_context(
        &self,
        state: &mut ConnectState,
        mut context: Context,
    ) -> Result<Option<Vec<ResolveContext>>, Error> {
        let (next, _, _) = self.find_next().ok_or(ContextResolverError::NoNext)?;

        let remaining = match next.action {
            // Cadence patch: autoplay refills go to the autoplay context, all pages in one.
            ContextAction::Append if next.update == ContextType::Autoplay => {
                let page = std::mem::take(&mut context.pages)
                    .into_iter()
                    .reduce(|mut a, b| {
                        a.tracks.extend(b.tracks);
                        a
                    })
                    .unwrap_or_default();
                state
                    .fill_context_from_page(page, ContextType::Autoplay)
                    .map(|_| None)
            }
            ContextAction::Append if context.pages.len() == 1 => state
                .fill_context_from_page(context.pages.remove(0), next.update)
                .map(|_| None),
            ContextAction::Replace => {
                let remaining = state.update_context(context, next.update);
                if let Resolve::Context(ref ctx) = next.resolve {
                    state.merge_context(ctx.pages.clone().pop());
                }

                remaining
            }
            ContextAction::Append => {
                warn!("unexpected page size: {context:#?}");
                Err(ContextResolverError::UnexpectedPagesSize(context.pages.len()).into())
            }
        }?;

        Ok(remaining.map(|remaining| {
            remaining
                .into_iter()
                .map(ResolveContext::append_context)
                .collect::<Vec<_>>()
        }))
    }

    pub fn try_finish(
        &self,
        state: &mut ConnectState,
        transfer_state: &mut Option<TransferState>,
    ) -> bool {
        let (next, _, _) = match self.find_next() {
            None => return false,
            Some(next) => next,
        };

        // when there is only one update type, we are the last of our kind, so we should update the state
        if self
            .queue
            .iter()
            .filter(|resolve| resolve.update == next.update)
            .count()
            != 1
        {
            return false;
        }

        match (next.update, state.active_context) {
            (ContextType::Default, ContextType::Default) | (ContextType::Autoplay, _) => {
                debug!(
                    "last item of type <{:?}>, finishing state setup",
                    next.update
                );
            }
            (ContextType::Default, _) => {
                debug!("skipped finishing default, because it isn't the active context");
                return false;
            }
        }

        let active_ctx = state.get_context(state.active_context);
        let res = if let Some(transfer_state) = transfer_state.take() {
            state.finish_transfer(transfer_state)
        } else if state.shuffling_context() && next.update == ContextType::Default {
            state.shuffle_new()
        } else if matches!(active_ctx, Ok(ctx) if ctx.index.track == 0) {
            // has context, and context is not touched
            // when the index is not zero, the next index was already evaluated elsewhere
            let ctx = active_ctx.expect("checked by precondition");
            let idx = ConnectState::find_index_in_context(ctx, |t| {
                state.current_track(|c| t.uri == c.uri)
            })
            .ok();

            state.reset_playback_to_position(idx)
        } else {
            state.fill_up_next_tracks()
        };

        if let Err(why) = res {
            error!("setup of state failed: {why}, last used resolve {next:#?}")
        }

        state.update_restrictions();
        state.update_queue_revision();

        true
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::*;

    #[test]
    fn autoplay_backoff_doubles_and_caps() {
        assert_eq!(autoplay_backoff(1), Duration::from_secs(5));
        assert_eq!(autoplay_backoff(2), Duration::from_secs(10));
        assert_eq!(autoplay_backoff(3), Duration::from_secs(20));
        assert_eq!(autoplay_backoff(7), Duration::from_secs(300));
        assert_eq!(autoplay_backoff(u32::MAX), Duration::from_secs(300));
        assert_eq!(autoplay_backoff(0), Duration::from_secs(5));
    }
}
