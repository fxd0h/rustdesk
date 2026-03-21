use crate::{
    common::{
        wayland,
        x11::{self},
        TraitCapturer,
    },
    Frame,
};
use std::{io, time::Duration};

#[cfg(target_os = "linux")]
use super::drm;

pub enum Capturer {
    X11(x11::Capturer),
    WAYLAND(wayland::Capturer),
    #[cfg(target_os = "linux")]
    DRM(drm::Capturer),
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        Ok(match display {
            Display::X11(d) => Capturer::X11(x11::Capturer::new(d)?),
            Display::WAYLAND(d) => Capturer::WAYLAND(wayland::Capturer::new(d)?),
            #[cfg(target_os = "linux")]
            Display::DRM(d) => Capturer::DRM(drm::Capturer::new(d)?),
        })
    }

    pub fn width(&self) -> usize {
        match self {
            Capturer::X11(d) => d.width(),
            Capturer::WAYLAND(d) => d.width(),
            #[cfg(target_os = "linux")]
            Capturer::DRM(d) => d.width(),
        }
    }

    pub fn height(&self) -> usize {
        match self {
            Capturer::X11(d) => d.height(),
            Capturer::WAYLAND(d) => d.height(),
            #[cfg(target_os = "linux")]
            Capturer::DRM(d) => d.height(),
        }
    }
}

impl TraitCapturer for Capturer {
    fn frame<'a>(&'a mut self, timeout: Duration) -> io::Result<Frame<'a>> {
        match self {
            Capturer::X11(d) => d.frame(timeout),
            Capturer::WAYLAND(d) => d.frame(timeout),
            #[cfg(target_os = "linux")]
            Capturer::DRM(d) => d.frame(timeout),
        }
    }
}

pub enum Display {
    X11(x11::Display),
    WAYLAND(wayland::Display),
    #[cfg(target_os = "linux")]
    DRM(drm::Display),
}

impl Display {
    pub fn primary() -> io::Result<Display> {
        // Try DRM first (no user consent popup, works on login screen)
        #[cfg(target_os = "linux")]
        if let Ok(d) = drm::Display::primary() {
            log::info!("[DRM] Using DRM/KMS capture");
            return Ok(Display::DRM(d));
        }

        Ok(if super::is_x11() {
            Display::X11(x11::Display::primary()?)
        } else {
            Display::WAYLAND(wayland::Display::primary()?)
        })
    }

    pub fn all() -> io::Result<Vec<Display>> {
        // Try DRM first
        #[cfg(target_os = "linux")]
        if let Ok(displays) = drm::Display::all() {
            if !displays.is_empty() {
                log::info!("[DRM] Using DRM/KMS capture ({} displays)", displays.len());
                return Ok(displays.into_iter().map(Display::DRM).collect());
            }
        }

        Ok(if super::is_x11() {
            x11::Display::all()?
                .drain(..)
                .map(|x| Display::X11(x))
                .collect()
        } else {
            wayland::Display::all()?
                .drain(..)
                .map(|x| Display::WAYLAND(x))
                .collect()
        })
    }

    pub fn width(&self) -> usize {
        match self {
            Display::X11(d) => d.width(),
            Display::WAYLAND(d) => d.width(),
            #[cfg(target_os = "linux")]
            Display::DRM(d) => d.width(),
        }
    }

    pub fn height(&self) -> usize {
        match self {
            Display::X11(d) => d.height(),
            Display::WAYLAND(d) => d.height(),
            #[cfg(target_os = "linux")]
            Display::DRM(d) => d.height(),
        }
    }

    pub fn scale(&self) -> f64 {
        match self {
            Display::X11(_d) => 1.0,
            Display::WAYLAND(d) => d.scale(),
            #[cfg(target_os = "linux")]
            Display::DRM(d) => d.scale(),
        }
    }

    pub fn logical_width(&self) -> usize {
        match self {
            Display::X11(d) => d.width(),
            Display::WAYLAND(d) => d.logical_width(),
            #[cfg(target_os = "linux")]
            Display::DRM(d) => d.logical_width(),
        }
    }

    pub fn logical_height(&self) -> usize {
        match self {
            Display::X11(d) => d.height(),
            Display::WAYLAND(d) => d.logical_height(),
            #[cfg(target_os = "linux")]
            Display::DRM(d) => d.logical_height(),
        }
    }

    pub fn origin(&self) -> (i32, i32) {
        match self {
            Display::X11(d) => d.origin(),
            Display::WAYLAND(d) => d.origin(),
            #[cfg(target_os = "linux")]
            Display::DRM(d) => d.origin(),
        }
    }

    pub fn is_online(&self) -> bool {
        match self {
            Display::X11(d) => d.is_online(),
            Display::WAYLAND(d) => d.is_online(),
            #[cfg(target_os = "linux")]
            Display::DRM(d) => d.is_online(),
        }
    }

    pub fn is_primary(&self) -> bool {
        match self {
            Display::X11(d) => d.is_primary(),
            Display::WAYLAND(d) => d.is_primary(),
            #[cfg(target_os = "linux")]
            Display::DRM(d) => d.is_primary(),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Display::X11(d) => d.name(),
            Display::WAYLAND(d) => d.name(),
            #[cfg(target_os = "linux")]
            Display::DRM(d) => d.name(),
        }
    }
}
