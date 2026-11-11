use crate::{
    core::{Error, SpotifyUri},
    protocol::player::ProvidedTrack,
    state::{
        ConnectState, SPOTIFY_MAX_NEXT_TRACKS_SIZE, SPOTIFY_MAX_PREV_TRACKS_SIZE, StateError,
        context::ContextType,
        metadata::Metadata,
        provider::{IsProvider, Provider},
    },
};
use protobuf::MessageField;
use rand::Rng;

// identifier used as part of the uid
pub const IDENTIFIER_DELIMITER: &str = "delimiter";

impl<'ct> ConnectState {
    fn new_delimiter(iteration: i64) -> ProvidedTrack {
        let mut delimiter = ProvidedTrack {
            uri: format!("spotify:{IDENTIFIER_DELIMITER}"),
            uid: format!("{IDENTIFIER_DELIMITER}{iteration}"),
            provider: Provider::Context.to_string(),
            ..Default::default()
        };
        delimiter.set_hidden(true);
        delimiter.set_iteration(iteration);

        delimiter
    }

    fn push_prev(&mut self, prev: ProvidedTrack) {
        let prev_tracks = self.prev_tracks_mut();
        // add prev track, while preserving a length of 10
        if prev_tracks.len() >= SPOTIFY_MAX_PREV_TRACKS_SIZE {
            // todo: O(n), but technically only maximal O(SPOTIFY_MAX_PREV_TRACKS_SIZE) aka O(10)
            let _ = prev_tracks.remove(0);
        }
        prev_tracks.push(prev)
    }

    fn get_next_track(&mut self) -> Option<ProvidedTrack> {
        if self.next_tracks().is_empty() {
            None
        } else {
            // todo: O(n), but technically only maximal O(SPOTIFY_MAX_NEXT_TRACKS_SIZE) aka O(80)
            Some(self.next_tracks_mut().remove(0))
        }
    }

    /// bottom => top, aka the last track of the list is the prev track
    fn prev_tracks_mut(&mut self) -> &mut Vec<ProvidedTrack> {
        &mut self.player_mut().prev_tracks
    }

    /// bottom => top, aka the last track of the list is the prev track
    pub(super) fn prev_tracks(&self) -> &Vec<ProvidedTrack> {
        &self.player().prev_tracks
    }

    /// top => bottom, aka the first track of the list is the next track
    fn next_tracks_mut(&mut self) -> &mut Vec<ProvidedTrack> {
        &mut self.player_mut().next_tracks
    }

    /// top => bottom, aka the first track of the list is the next track
    pub(super) fn next_tracks(&self) -> &Vec<ProvidedTrack> {
        &self.player().next_tracks
    }

    pub fn set_current_track_random(&mut self) -> Result<(), Error> {
        let max_tracks = self.get_context(self.active_context)?.tracks.len();
        let rng_track = rand::rng().random_range(0..max_tracks);
        self.set_current_track(rng_track)
    }

    pub fn set_current_track(&mut self, index: usize) -> Result<(), Error> {
        let context = self.get_context(self.active_context)?;

        let new_track = context
            .tracks
            .get(index)
            .ok_or(StateError::CanNotFindTrackInContext(
                Some(index),
                context.tracks.len(),
            ))?;

        debug!(
            "set track to: {} at {} of {} tracks",
            new_track.uri,
            index,
            context.tracks.len()
        );

        self.set_track(new_track.clone());

        self.update_current_index(|i| i.track = index as u32);

        Ok(())
    }

    /// Move to the next track
    ///
    /// Updates the current track to the next track. Adds the old track
    /// to prev tracks and fills up the next tracks from the current context
    pub fn next_track(&mut self) -> Result<Option<u32>, Error> {
        // when we skip in repeat track, we don't repeat the current track anymore
        if self.repeat_track() {
            self.set_repeat_track(false);
        }

        let old_track = self.player_mut().track.take();

        if let Some(old_track) = old_track {
            self.cadence_mark_played(&old_track.uri);
            // only add songs from our context to our previous tracks
            if old_track.is_context() || old_track.is_autoplay() {
                self.push_prev(old_track)
            }
        }

        let new_track = loop {
            match self.get_next_track() {
                Some(next) if next.uid.starts_with(IDENTIFIER_DELIMITER) => {
                    self.push_prev(next);
                    continue;
                }
                Some(next) if next.is_unavailable() => continue,
                other => break other,
            };
        };

        let new_track = match new_track {
            None => return Ok(None),
            Some(t) => t,
        };

        self.fill_up_next_tracks()?;

        let update_index = if new_track.is_queue() {
            None
        } else if new_track.is_autoplay() {
            self.set_active_context(ContextType::Autoplay);
            None
        } else {
            match new_track.get_context_index() {
                Some(new_index) => Some(new_index as u32),
                None => {
                    error!("the given context track had no set context_index");
                    None
                }
            }
        };

        if let Some(update_index) = update_index {
            self.update_current_index(|i| i.track = update_index)
        } else {
            self.player_mut().index.clear()
        }

        self.set_track(new_track);
        self.update_restrictions();

        Ok(Some(self.player().index.track))
    }

    /// Move to the prev track
    ///
    /// Updates the current track to the prev track. Adds the old track
    /// to next tracks (when from the context) and fills up the prev tracks from the
    /// current context
    pub fn prev_track(&mut self) -> Result<Option<&MessageField<ProvidedTrack>>, Error> {
        let old_track = self.player_mut().track.take();

        if let Some(old_track) = old_track {
            if old_track.is_context() || old_track.is_autoplay() {
                // todo: O(n)
                self.next_tracks_mut().insert(0, old_track);
            }
        }

        // handle possible delimiter
        if matches!(self.prev_tracks().last(), Some(prev) if prev.uid.starts_with(IDENTIFIER_DELIMITER))
        {
            let delimiter = self
                .prev_tracks_mut()
                .pop()
                .expect("item that was prechecked");

            let next_tracks = self.next_tracks_mut();
            if next_tracks.len() >= SPOTIFY_MAX_NEXT_TRACKS_SIZE {
                let _ = next_tracks.pop();
            }
            // todo: O(n)
            next_tracks.insert(0, delimiter)
        }

        while self.next_tracks().len() > SPOTIFY_MAX_NEXT_TRACKS_SIZE {
            let _ = self.next_tracks_mut().pop();
        }

        let new_track = match self.prev_tracks_mut().pop() {
            None => return Ok(None),
            Some(t) => t,
        };

        if matches!(self.active_context, ContextType::Autoplay if new_track.is_context()) {
            // transition back to default context
            self.set_active_context(ContextType::Default);
        }

        self.fill_up_next_tracks()?;
        self.set_track(new_track);

        if self.player().index.track == 0 {
            warn!("prev: trying to skip into negative, index update skipped")
        } else {
            self.update_current_index(|i| i.track -= 1)
        }

        self.update_restrictions();

        Ok(Some(self.current_track(|t| t)))
    }

    pub fn current_track<F: Fn(&'ct MessageField<ProvidedTrack>) -> R, R>(
        &'ct self,
        access: F,
    ) -> R {
        access(&self.player().track)
    }

    pub fn set_track(&mut self, track: ProvidedTrack) {
        self.player_mut().track = MessageField::some(track)
    }

    pub fn set_next_tracks(&mut self, mut tracks: Vec<ProvidedTrack>) {
        // mobile only sends a set_queue command instead of an add_to_queue command
        // in addition to handling the mobile add_to_queue handling, this should also handle
        // a mass queue addition
        tracks
            .iter_mut()
            .filter(|t| t.is_from_queue())
            .for_each(|t| {
                t.set_provider(Provider::Queue);
                // technically we could preserve the queue-uid here,
                // but it seems to work without that, so we just override it
                t.uid = format!("q{}", self.queue_count);
                self.queue_count += 1;
            });

        // when you drag 'n drop the current track in the queue view into the "Next from: ..."
        // section, it is only send as an empty item with just the provider and metadata, so we have
        // to provide set the uri from the current track manually
        tracks
            .iter_mut()
            .filter(|t| t.uri.is_empty())
            .for_each(|t| t.uri = self.current_track(|ct| ct.uri.clone()));

        self.player_mut().next_tracks = tracks;
    }

    pub fn set_prev_tracks(&mut self, tracks: Vec<ProvidedTrack>) {
        self.player_mut().prev_tracks = tracks;
    }

    pub fn clear_prev_track(&mut self) {
        self.prev_tracks_mut().clear()
    }

    pub fn clear_next_tracks(&mut self) {
        // respect queued track and don't throw them out of our next played tracks
        let first_non_queued_track = self
            .next_tracks()
            .iter()
            .enumerate()
            .find(|(_, track)| !track.is_queue());

        if let Some((non_queued_track, _)) = first_non_queued_track {
            while self.next_tracks().len() > non_queued_track
                && self.next_tracks_mut().pop().is_some()
            {}
        }
    }

    pub fn fill_up_next_tracks(&mut self) -> Result<(), Error> {
        let ctx = self.get_context(self.fill_up_context)?;
        let mut new_index = ctx.index.track as usize;
        let mut iteration = ctx.index.page;

        while self.next_tracks().len() < SPOTIFY_MAX_NEXT_TRACKS_SIZE {
            let ctx = self.get_context(self.fill_up_context)?;
            let track = match ctx.tracks.get(new_index) {
                None if self.repeat_context() => {
                    let delimiter = Self::new_delimiter(iteration.into());
                    iteration += 1;
                    new_index = 0;
                    delimiter
                }
                None if !matches!(self.fill_up_context, ContextType::Autoplay)
                    && self.autoplay_context.is_some()
                    && !self.repeat_context() =>
                {
                    self.update_context_index(self.fill_up_context, new_index)?;

                    // transition to autoplay as fill up context
                    self.fill_up_context = ContextType::Autoplay;
                    new_index = self.get_context(ContextType::Autoplay)?.index.track as usize;

                    // add delimiter to only display the current context
                    Self::new_delimiter(iteration.into())
                }
                None if self.autoplay_context.is_some() => {
                    match self
                        .get_context(ContextType::Autoplay)?
                        .tracks
                        .get(new_index)
                    {
                        None => break,
                        Some(ct) => {
                            new_index += 1;
                            ct.clone()
                        }
                    }
                }
                None => break,
                Some(ct) if ct.is_unavailable() || self.is_skip_track(ct, Some(iteration)) => {
                    debug!(
                        "skipped track {} during fillup as it's unavailable or should be skipped",
                        ct.uri
                    );
                    new_index += 1;
                    continue;
                }
                Some(ct) => {
                    new_index += 1;
                    ct.clone()
                }
            };

            self.next_tracks_mut().push(track);
        }

        debug!(
            "finished filling up next_tracks ({})",
            self.next_tracks().len()
        );

        self.update_context_index(self.fill_up_context, new_index)?;

        // the web-player needs a revision update, otherwise the queue isn't updated in the ui
        self.update_queue_revision();

        Ok(())
    }

    pub fn preview_next_track(&mut self) -> Option<SpotifyUri> {
        let next = if self.repeat_track() {
            self.current_track(|t| &t.uri)
        } else {
            &self.next_tracks().first()?.uri
        };

        SpotifyUri::from_uri(next).ok()
    }

    pub fn has_next_tracks(&self, min: Option<usize>) -> bool {
        if let Some(min) = min {
            self.next_tracks().len() >= min
        } else {
            !self.next_tracks().is_empty()
        }
    }

    pub fn recent_track_uris(&self) -> Vec<String> {
        let mut prev = self
            .prev_tracks()
            .iter()
            .map(|t| t.uri.clone())
            .collect::<Vec<_>>();

        prev.push(self.current_track(|t| t.uri.clone()));
        prev
    }

    pub fn mark_unavailable(&mut self, id: &SpotifyUri) -> Result<(), Error> {
        let uri = id.to_uri()?;

        debug!("marking {uri} as unavailable");

        let next_tracks = self.next_tracks_mut();
        while let Some(pos) = next_tracks.iter().position(|t| t.uri == uri) {
            let _ = next_tracks.remove(pos);
        }

        for next_track in next_tracks {
            Self::mark_as_unavailable_for_match(next_track, &uri)
        }

        let prev_tracks = self.prev_tracks_mut();
        while let Some(pos) = prev_tracks.iter().position(|t| t.uri == uri) {
            let _ = prev_tracks.remove(pos);
        }

        for prev_track in prev_tracks {
            Self::mark_as_unavailable_for_match(prev_track, &uri)
        }

        self.unavailable_uri.push(uri);
        self.fill_up_next_tracks()?;
        self.update_queue_revision();

        Ok(())
    }

    pub fn add_to_queue(&mut self, mut track: ProvidedTrack, rev_update: bool) {
        track.uid = format!("q{}", self.queue_count);
        self.queue_count += 1;

        track.set_provider(Provider::Queue);
        if !track.is_from_queue() {
            track.set_from_queue(true);
        }

        let next_tracks = self.next_tracks_mut();
        if let Some(next_not_queued_track) = next_tracks.iter().position(|t| !t.is_queue()) {
            next_tracks.insert(next_not_queued_track, track);
        } else {
            next_tracks.push(track)
        }

        while next_tracks.len() > SPOTIFY_MAX_NEXT_TRACKS_SIZE {
            next_tracks.pop();
        }

        if rev_update {
            self.update_queue_revision();
        }
        self.update_restrictions();
    }

    // --- Cadence patch: queue editing for the local client (see CADENCE_PATCH.md) ---

    /// Upcoming tracks (queue first, then context; delimiters left out) and recent ones.
    pub fn cadence_snapshot(&self) -> crate::QueueSnapshot {
        let item = |t: &ProvidedTrack| crate::QueueItem {
            uri: t.uri.clone(),
            uid: t.uid.clone(),
            queued: t.is_queue(),
            autoplay: t.is_autoplay(),
        };
        let real = |t: &&ProvidedTrack| !t.uid.starts_with(IDENTIFIER_DELIMITER) && !t.uri.is_empty();
        crate::QueueSnapshot {
            current: self.player().track.as_ref().map(item),
            next: self.next_tracks().iter().filter(real).map(item).collect(),
            prev: self.prev_tracks().iter().filter(real).map(item).collect(),
        }
    }

    /// Inserts a track into the queue segment at `position` (clamped; `None` = end of queue).
    pub fn cadence_queue_insert(&mut self, uri: String, position: Option<usize>) {
        self.add_to_queue(ProvidedTrack { uri, ..Default::default() }, false);
        if let Some(pos) = position {
            let queued = self.next_tracks().iter().take_while(|t| t.is_queue()).count();
            if queued > 0 {
                let track = self.next_tracks_mut().remove(queued - 1);
                self.next_tracks_mut().insert(pos.min(queued - 1), track);
            }
        }
        self.update_queue_revision();
    }

    /// Removes the queued track with `uid` (context tracks can only be skipped, not removed).
    pub fn cadence_queue_remove(&mut self, uid: &str) {
        self.next_tracks_mut().retain(|t| !(t.is_queue() && t.uid == uid));
        self.update_queue_revision();
        self.update_restrictions();
    }

    /// Moves the queued track with `uid` to `position` inside the queue segment.
    pub fn cadence_queue_move(&mut self, uid: &str, position: usize) {
        let queued = self.next_tracks().iter().take_while(|t| t.is_queue()).count();
        let Some(from) = self.next_tracks().iter().take(queued).position(|t| t.uid == uid) else { return };
        let track = self.next_tracks_mut().remove(from);
        self.next_tracks_mut().insert(position.min(queued - 1), track);
        self.update_queue_revision();
    }

    /// Drops every queued track; the context continues.
    pub fn cadence_queue_clear(&mut self) {
        self.next_tracks_mut().retain(|t| !t.is_queue());
        self.update_queue_revision();
        self.update_restrictions();
    }

    /// Cadence patch: remembers a played uri in the bounded ring buffer.
    pub fn cadence_mark_played(&mut self, uri: &str) {
        if uri.is_empty() || !uri.starts_with("spotify:") {
            return;
        }
        cadence_push_ring(&mut self.cadence_played, uri, super::CADENCE_PLAYED_MAX);
    }

    /// Cadence patch: the uri to ask autoplay for. The context itself, except for track lists
    /// (`spotify:web-api`), which the autoplay endpoint can't resolve: then the playing track,
    /// or the latest played one when that is a local file. `None`: nothing to seed from.
    pub fn cadence_autoplay_seed(&self) -> Option<String> {
        let ctx = self.context_uri();
        if ctx.is_empty() {
            return None;
        }
        if ctx != CADENCE_TRACK_LIST_URI {
            return Some(ctx.clone());
        }
        let current = self.current_track(|t| t.uri.clone());
        std::iter::once(current)
            .chain(self.prev_tracks().iter().rev().map(|t| t.uri.clone()))
            .chain(self.cadence_played.iter().rev().cloned())
            .find(|u| cadence_seedable(u))
    }

    /// Cadence patch: drops autoplay tracks played this session, in the context that just
    /// ended, already waiting in the autoplay context, marked unavailable, or listed twice.
    /// Returns how many are left.
    pub fn cadence_filter_autoplay(&self, tracks: &mut Vec<crate::protocol::context_track::ContextTrack>) -> usize {
        use std::collections::HashSet;
        let mut seen: HashSet<&str> = self.cadence_played.iter().map(String::as_str).collect();
        seen.extend(self.unavailable_uri.iter().map(String::as_str));
        seen.insert(self.current_track(|t| t.uri.as_str()));
        if let Some(ctx) = self.context.as_ref() {
            seen.extend(ctx.tracks.iter().map(|t| t.uri.as_str()));
        }
        if let Some(ctx) = self.autoplay_context.as_ref() {
            seen.extend(ctx.tracks.iter().skip(ctx.index.track as usize).map(|t| t.uri.as_str()));
        }
        let mut kept: HashSet<String> = HashSet::new();
        tracks.retain(|t| match t.uri.as_deref() {
            // gid-only items can't be compared here; librespot converts them later
            None | Some("") => true,
            Some(uri) => !seen.contains(uri) && kept.insert(uri.to_string()),
        });
        tracks.len()
    }

    /// Cadence patch: frees autoplay tracks that were already moved to next_tracks, keeping one
    /// so the context's index stays non-zero (librespot reads index 0 as "untouched").
    pub fn cadence_trim_autoplay(&mut self) {
        if let Some(ctx) = self.autoplay_context.as_mut() {
            let idx = (ctx.index.track as usize).min(ctx.tracks.len());
            if idx > 1 {
                ctx.tracks.drain(0..idx - 1);
                ctx.index.track = 1;
            }
        }
    }
}

/// Cadence patch: the context uri librespot gives track-list loads.
pub(crate) const CADENCE_TRACK_LIST_URI: &str = "spotify:web-api";

/// Cadence patch: a track the autoplay endpoint accepts as a seed (no local files, no episodes).
pub(crate) fn cadence_seedable(uri: &str) -> bool {
    uri.starts_with("spotify:track:")
}

/// Cadence patch: appends `uri` (moving it to the end if present), keeping at most `max`.
pub(crate) fn cadence_push_ring(ring: &mut std::collections::VecDeque<String>, uri: &str, max: usize) {
    if let Some(i) = ring.iter().position(|u| u == uri) {
        ring.remove(i);
    }
    ring.push_back(uri.to_string());
    while ring.len() > max {
        ring.pop_front();
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn ring_is_bounded_and_moves_repeats_to_the_end() {
        let mut ring = VecDeque::new();
        for i in 0..500 {
            cadence_push_ring(&mut ring, &format!("spotify:track:{i}"), 200);
        }
        assert_eq!(ring.len(), 200);
        assert_eq!(ring.front().map(String::as_str), Some("spotify:track:300"));
        cadence_push_ring(&mut ring, "spotify:track:300", 200);
        assert_eq!(ring.len(), 200);
        assert_eq!(ring.back().map(String::as_str), Some("spotify:track:300"));
        assert_eq!(ring.front().map(String::as_str), Some("spotify:track:301"));
    }

    #[test]
    fn only_spotify_tracks_seed_autoplay() {
        assert!(cadence_seedable("spotify:track:4uLU6hMCjMI75M1A2tKUQC"));
        assert!(!cadence_seedable("spotify:local:a:b:c:180"));
        assert!(!cadence_seedable("spotify:episode:xyz"));
        assert!(!cadence_seedable(""));
    }
}
