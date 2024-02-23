use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use color_eyre::Result;
use image::imageops::FilterType;
use image::{DynamicImage, ImageBuffer, Pixel, Rgba};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{LoopHandle, RegistrationToken};
use smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput;
use smithay_client_toolkit::reexports::client::protocol::wl_surface;
use smithay_client_toolkit::reexports::client::QueueHandle;
use smithay_client_toolkit::shell::wlr_layer::{LayerSurface, LayerSurfaceConfigure};

use crate::filelist_cache::FilelistCache;
use crate::image_picker::ImagePicker;
use crate::render::{EglContext, Renderer};
use crate::wallpaper_info::{BackgroundMode, WallpaperInfo};
use crate::wpaperd::Wpaperd;

pub struct Surface {
    pub name: String,
    pub surface: wl_surface::WlSurface,
    pub output: WlOutput,
    pub layer: LayerSurface,
    pub width: u32,
    pub height: u32,
    pub scale: i32,
    egl_context: EglContext,
    renderer: Renderer,
    pub image_picker: ImagePicker,
    pub event_source: Option<RegistrationToken>,
    wallpaper_info: Arc<WallpaperInfo>,
}

impl Surface {
    pub fn new(
        name: String,
        layer: LayerSurface,
        output: WlOutput,
        surface: wl_surface::WlSurface,
        scale_factor: i32,
        wallpaper_info: Arc<WallpaperInfo>,
        egl_display: egl::Display,
        filelist_cache: Rc<RefCell<FilelistCache>>,
    ) -> Self {
        let egl_context = EglContext::new(egl_display, &surface);
        // Make the egl context as current to make the renderer creation work
        egl_context.make_current().unwrap();

        // Commit the surface
        surface.commit();

        let image_picker = ImagePicker::new(wallpaper_info.clone(), filelist_cache);

        let image = image::open(image_picker.current_image())
            .unwrap()
            .into_rgba8();
        let renderer = unsafe { Renderer::new(image.into()).unwrap() };

        Self {
            name,
            output,
            layer,
            width: 0,
            height: 0,
            scale: scale_factor,
            surface,
            egl_context,
            renderer,
            image_picker,
            event_source: None,
            wallpaper_info,
        }
    }

    /// Returns true if something has been drawn to the surface
    pub fn draw(&mut self, qh: &QueueHandle<Wpaperd>, time: u32) -> Result<()> {
        debug_assert!(self.width != 0 || self.height != 0);

        let width = self.width as i32 * self.scale;
        let height = self.height as i32 * self.scale;

        // Use the correct context before loading the texture and drawing
        self.egl_context.make_current()?;

        if let Some(mut image) = self.image_picker.get_image()? {
            let image = image.into_rgba8();
            self.renderer
                .load_texture(image.into(), self.wallpaper_info.mode)?;
            self.renderer.start_animation(time);

            // self.apply_shadow(&mut image, width.try_into()?);
        }
        if self.renderer.time_started == 0 {
            self.renderer.start_animation(time);
        }

        unsafe { self.renderer.draw(time)? };

        if self.is_drawing_animation(time) {
            self.queue_draw(qh);
        }

        self.renderer.clear_after_draw()?;
        self.egl_context.swap_buffers()?;

        // Reset the context
        egl::API
            .make_current(self.egl_context.display, None, None, None)
            .unwrap();

        // Mark the entire surface as damaged
        self.surface.damage_buffer(0, 0, width, height);

        // Finally, commit the surface
        self.surface.commit();

        Ok(())
    }

    fn apply_shadow(&self, image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>, width: u32) {
        if self.wallpaper_info.apply_shadow.unwrap_or_default() {
            const GRADIENT_HEIGHT: u32 = 11;
            type RgbaImage = image::ImageBuffer<image::Rgba<u8>, Vec<u8>>;
            let gradient = DynamicImage::ImageRgba8(
                RgbaImage::from_raw(
                    1,
                    GRADIENT_HEIGHT,
                    vec![
                        0, 0, 0, 225, 0, 0, 0, 202, 0, 0, 0, 178, 0, 0, 0, 154, 0, 0, 0, 130, 0, 0,
                        0, 107, 0, 0, 0, 83, 0, 0, 0, 59, 0, 0, 0, 36, 0, 0, 0, 12, 0, 0, 0, 0,
                    ],
                )
                .unwrap(),
            )
            .resize_exact(
                width,
                GRADIENT_HEIGHT * 4 * self.scale as u32,
                FilterType::Triangle,
            )
            .into_rgba8();

            image
                .pixels_mut()
                .zip(gradient.pixels())
                .for_each(|(p, g)| p.blend(g));
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Resize the surface
    /// configure: None means that the scale factor has changed
    pub fn resize(&mut self, configure: Option<LayerSurfaceConfigure>) {
        if let Some(configure) = configure {
            (self.width, self.height) = configure.new_size;
        }
        let width = self.width as i32 * self.scale;
        let height = self.height as i32 * self.scale;
        self.egl_context.resize(&self.surface, width, height);
        // Resize the gl viewport
        self.egl_context.make_current().unwrap();
        self.renderer.resize(width, height).unwrap();
    }

    /// Check that the dimensions are valid
    pub(crate) fn is_configured(&self) -> bool {
        self.width != 0 && self.height != 0
    }

    /// Update the wallpaper_info of this Surface
    /// return true if the duration has changed
    pub fn update_wallpaper_info(
        &mut self,
        handle: &LoopHandle<Wpaperd>,
        qh: &QueueHandle<Wpaperd>,
        mut wallpaper_info: Arc<WallpaperInfo>,
    ) {
        if self.wallpaper_info != wallpaper_info {
            // Put the new value in place
            std::mem::swap(&mut self.wallpaper_info, &mut wallpaper_info);
            let path_changed = self.image_picker.update(&*self.wallpaper_info);
            if self.wallpaper_info.duration != wallpaper_info.duration {
                match (self.wallpaper_info.duration, wallpaper_info.duration) {
                    (None, None) => {
                        unreachable!()
                    }
                    // There was a duration before but now it has been removed
                    (None, Some(_)) => {
                        if let Some(registration_token) = self.event_source.take() {
                            handle.remove(registration_token);
                        }
                        if path_changed {
                            self.queue_draw(qh);
                        }
                    }
                    // There wasn't a duration before but now it has been added or it has changed
                    (Some(new_duration), None) | (Some(new_duration), Some(_)) => {
                        if let Some(registration_token) = self.event_source.take() {
                            handle.remove(registration_token);
                        }

                        // if the path has not changed or the duration has changed
                        // and the remaining time is great than 0
                        let timer = if let (false, Some(remaining_time)) = (
                            path_changed,
                            remaining_duration(
                                new_duration,
                                self.image_picker.image_changed_instant,
                            ),
                        ) {
                            Some(Timer::from_duration(remaining_time))
                        } else {
                            // otherwise draw the image immediately, the next timer
                            // will be set to the new duration
                            Some(Timer::immediate())
                        };

                        self.add_timer(timer, handle, qh.clone());
                    }
                }
            } else {
                if path_changed {
                    self.queue_draw(qh);
                }
            }
        }
    }

    /// Add a new timer in the event_loop for the current duration
    /// Stop if there is already a timer added
    pub fn add_timer(
        &mut self,
        timer: Option<Timer>,
        handle: &LoopHandle<Wpaperd>,
        qh: QueueHandle<Wpaperd>,
    ) {
        if let Some(duration) = self.wallpaper_info.duration {
            let timer = timer.unwrap_or(Timer::from_duration(duration));
            if self.event_source.is_some() {
                return;
            }

            let name = self.name.clone();
            let registration_token = handle
                .insert_source(
                    timer,
                    move |_deadline, _: &mut (), wpaperd: &mut Wpaperd| {
                        // TODO: error handling
                        let surface = wpaperd.surface_from_name(&name).unwrap();
                        if let Some(duration) = surface.wallpaper_info.duration {
                            // Check that the timer has expired
                            // if the daemon received a next or previous image command
                            // the timer will be reset and we need to account that here
                            // i.e. there is a timer of 1 minute. The user changes the image
                            // with a previous wallpaper command at 50 seconds.
                            // The timer will be reset to 1 minute and the image will be changed
                            if let Some(remaining_time) = remaining_duration(
                                duration,
                                surface.image_picker.image_changed_instant,
                            ) {
                                TimeoutAction::ToDuration(remaining_time)
                            } else {
                                // Change the drawn image
                                surface.image_picker.next_image();
                                surface.queue_draw(&qh);
                                TimeoutAction::ToDuration(duration)
                            }
                        } else {
                            TimeoutAction::Drop
                        }
                    },
                )
                .expect("Failed to insert event source!");

            self.event_source = Some(registration_token);
        }
    }

    pub fn is_drawing_animation(&self, time: u32) -> bool {
        self.renderer.is_drawing_animation(time)
    }

    pub(crate) fn queue_draw(&self, qh: &QueueHandle<Wpaperd>) {
        self.surface.frame(qh, self.surface.clone());
        self.surface.commit();
    }
}

fn remaining_duration(duration: Duration, image_changed: Instant) -> Option<Duration> {
    // The timer has already expired
    let diff = image_changed.elapsed();
    if duration.saturating_sub(diff).is_zero() {
        None
    } else {
        Some(duration - diff)
    }
}
