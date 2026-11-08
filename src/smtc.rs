#![allow(dead_code)]

use windows::{
    Foundation::{TypedEventHandler, Uri},
    Media::{
        MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls,
        SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
    },
    Storage::Streams::RandomAccessStreamReference,
    Win32::{Foundation::HWND, System::WinRT::ISystemMediaTransportControlsInterop},
    core::{HSTRING, factory},
};

pub enum Key {
    PlayPause,
    Next,
    Prev,
}

/// Create and use on the UI thread, after the native window exists.
pub struct Smtc {
    controls: SystemMediaTransportControls,
    token: i64,
}

impl Smtc {
    pub fn new(hwnd: isize, on_key: impl Fn(Key) + Send + Sync + 'static) -> Result<Smtc, String> {
        let err = |e: windows::core::Error| e.to_string();
        let interop = factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()
            .map_err(err)?;
        let controls: SystemMediaTransportControls =
            unsafe { interop.GetForWindow(HWND(hwnd as *mut _)) }.map_err(err)?;
        controls.SetIsEnabled(true).map_err(err)?;
        controls.SetIsPlayEnabled(true).map_err(err)?;
        controls.SetIsPauseEnabled(true).map_err(err)?;
        controls.SetIsNextEnabled(true).map_err(err)?;
        controls.SetIsPreviousEnabled(true).map_err(err)?;
        let token = controls
            .ButtonPressed(&TypedEventHandler::<
                SystemMediaTransportControls,
                SystemMediaTransportControlsButtonPressedEventArgs,
            >::new(move |_, args| {
                if let Some(args) = args.as_ref() {
                    match args.Button()? {
                        SystemMediaTransportControlsButton::Play
                        | SystemMediaTransportControlsButton::Pause => on_key(Key::PlayPause),
                        SystemMediaTransportControlsButton::Next => on_key(Key::Next),
                        SystemMediaTransportControlsButton::Previous => on_key(Key::Prev),
                        _ => {}
                    }
                }
                Ok(())
            }))
            .map_err(err)?;
        Ok(Smtc { controls, token })
    }

    pub fn update(&self, title: &str, artist: &str, playing: bool, art_url: Option<&str>) {
        let _ = self.try_update(title, artist, playing, art_url);
    }

    fn try_update(
        &self,
        title: &str,
        artist: &str,
        playing: bool,
        art_url: Option<&str>,
    ) -> windows::core::Result<()> {
        self.controls.SetPlaybackStatus(if playing {
            MediaPlaybackStatus::Playing
        } else {
            MediaPlaybackStatus::Paused
        })?;
        let updater = self.controls.DisplayUpdater()?;
        updater.SetType(MediaPlaybackType::Music)?;
        let _ = updater.SetAppMediaId(&HSTRING::from("Cadence"));
        let music = updater.MusicProperties()?;
        music.SetTitle(&HSTRING::from(title))?;
        music.SetArtist(&HSTRING::from(artist))?;
        if let Some(url) = art_url {
            if let Ok(uri) = Uri::CreateUri(&HSTRING::from(url)) {
                if let Ok(stream_ref) = RandomAccessStreamReference::CreateFromUri(&uri) {
                    let _ = updater.SetThumbnail(&stream_ref);
                }
            }
        }
        updater.Update()
    }
}

impl Drop for Smtc {
    fn drop(&mut self) {
        let _ = self.controls.RemoveButtonPressed(self.token);
    }
}
