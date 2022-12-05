// SPDX-License-Identifier: MPL-2.0-only

use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader},
    os::unix::net::UnixStream,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::bail;
use itertools::{chain, izip, Itertools};
use launch_pad::process::Process;
use sctk::{
    compositor::Region,
    output::OutputInfo,
    reexports::client::{
        backend::ObjectId,
        protocol::{wl_display::WlDisplay, wl_output as c_wl_output},
        Proxy, QueueHandle,
    },
    shell::{
        layer::{self, KeyboardInteractivity, Layer, LayerShell, LayerSurface},
        xdg::popup,
    },
};
use slog::{info, Logger};
use smithay::{
    backend::{
        egl::{context::GlAttributes, EGLContext},
        renderer::{
            damage::DamageTrackedRenderer,
            element::{
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                Element, RenderElement,
            },
            Bind, Frame, Renderer, Unbind,
        },
    },
    desktop::{
        space::{space_render_elements, RenderZindex, SpaceElement},
        utils::{
            bbox_from_surface_tree, surface_primary_scanout_output,
            update_surface_primary_scanout_output,
        },
    },
    output::Output,
    reexports::wayland_server::{
        backend::ClientId, protocol::wl_surface::WlSurface, DisplayHandle,
    },
    wayland::seat::WaylandFocus,
};
use smithay::{
    backend::{
        egl::{display::EGLDisplay, surface::EGLSurface},
        renderer::gles2::Gles2Renderer,
    },
    desktop::{PopupKind, PopupManager, Space, Window},
    reexports::wayland_server::{Client, Resource},
    utils::{Logical, Physical, Point, Rectangle, Size},
};
use tokio::sync::mpsc;
use wayland_egl::WlEglSurface;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1;
use xdg_shell_wrapper::{
    client_state::{ClientFocus, FocusStatus},
    server_state::{ServerFocus, ServerPtrFocus},
    shared_state::GlobalState,
    space::{ClientEglSurface, SpaceEvent, Visibility, WrapperPopup, WrapperSpace},
    util::smootherstep,
};

use cosmic_panel_config::{
    CosmicPanelBackground, CosmicPanelConfig, CosmicPanelOuput, PanelAnchor,
};

use crate::space::Alignment;
use xdg_shell_wrapper::space::WrapperSpaceElement;

pub enum AppletMsg {
    NewProcess(Process),
    ClientSocketPair(String, ClientId, Client, UnixStream),
}
/// space for the cosmic panel
#[derive(Debug)]
pub(crate) struct PanelSpace {
    // XXX implicitly drops egl_surface first to avoid segfault
    pub(crate) egl_surface: Option<Rc<EGLSurface>>,
    pub(crate) c_display: Option<WlDisplay>,
    pub config: CosmicPanelConfig,
    pub log: Logger,
    pub(crate) space: Space<Window>,
    pub(crate) damage_tracked_renderer: Option<DamageTrackedRenderer>,
    pub(crate) clients_left: Vec<(String, Client, UnixStream)>,
    pub(crate) clients_center: Vec<(String, Client, UnixStream)>,
    pub(crate) clients_right: Vec<(String, Client, UnixStream)>,
    pub(crate) last_dirty: Option<Instant>,
    pub(crate) pending_dimensions: Option<Size<i32, Logical>>,
    pub(crate) suggested_length: Option<u32>,
    pub(crate) actual_length: u32,
    pub(crate) full_clear: u8,
    pub(crate) is_dirty: bool,
    pub(crate) space_event: Rc<Cell<Option<SpaceEvent>>>,
    pub(crate) dimensions: Size<i32, Logical>,
    pub(crate) c_focused_surface: Rc<RefCell<ClientFocus>>,
    pub(crate) c_hovered_surface: Rc<RefCell<ClientFocus>>,
    pub(crate) s_focused_surface: ServerFocus,
    pub(crate) s_hovered_surface: ServerPtrFocus,
    pub(crate) visibility: Visibility,
    pub(crate) output: Option<(c_wl_output::WlOutput, Output, OutputInfo)>,
    pub(crate) s_display: Option<DisplayHandle>,
    pub(crate) layer: Option<LayerSurface>,
    pub(crate) popups: Vec<WrapperPopup>,
    pub(crate) start_instant: Instant,
    pub(crate) bg_color: [f32; 4],
    pub applet_tx: mpsc::Sender<AppletMsg>,
    pub(crate) input_region: Option<Region>,
}

impl PanelSpace {
    /// create a new space for the cosmic panel
    pub fn new(
        config: CosmicPanelConfig,
        log: Logger,
        c_focused_surface: Rc<RefCell<ClientFocus>>,
        c_hovered_surface: Rc<RefCell<ClientFocus>>,
        applet_tx: mpsc::Sender<AppletMsg>,
    ) -> Self {
        let bg_color = match config.background {
            CosmicPanelBackground::ThemeDefault(alpha) => {
                let path = xdg::BaseDirectories::with_prefix("gtk-4.0")
                    .ok()
                    .and_then(|xdg_dirs| xdg_dirs.find_config_file("cosmic.css"))
                    .unwrap_or_else(|| "~/.config/gtk-4.0/cosmic.css".into());
                let window_bg_color_pattern = "@define-color window_bg_color";
                if let Some(color) = File::open(path).ok().and_then(|file| {
                    BufReader::new(file)
                        .lines()
                        .filter_map(|l| l.ok())
                        .find_map(|line| {
                            line.rfind(window_bg_color_pattern)
                                .and_then(|i| line.get(i + window_bg_color_pattern.len()..))
                                .and_then(|color_str| {
                                    csscolorparser::parse(&color_str.trim().replace(";", "")).ok()
                                })
                        })
                }) {
                    [
                        color.r as f32,
                        color.g as f32,
                        color.b as f32,
                        alpha.unwrap_or(color.a as f32),
                    ]
                } else {
                    [0.0, 0.0, 0.0, alpha.unwrap_or(1.0)]
                }
            }
            CosmicPanelBackground::Color(c) => c,
        };

        Self {
            config,
            space: Space::new(log.clone()),
            log,
            full_clear: 0,
            clients_left: Default::default(),
            clients_center: Default::default(),
            clients_right: Default::default(),
            last_dirty: Default::default(),
            pending_dimensions: Default::default(),
            space_event: Default::default(),
            dimensions: Default::default(),
            suggested_length: None,
            output: Default::default(),
            s_display: Default::default(),
            c_display: Default::default(),
            layer: Default::default(),
            egl_surface: Default::default(),
            popups: Default::default(),
            visibility: Visibility::Visible,
            start_instant: Instant::now(),
            c_focused_surface,
            c_hovered_surface,
            s_focused_surface: Default::default(),
            s_hovered_surface: Default::default(),
            bg_color,
            applet_tx,
            actual_length: 0,
            input_region: None,
            damage_tracked_renderer: Default::default(),
            is_dirty: false,
        }
    }

    pub(crate) fn close_popups(&mut self) {
        for w in &mut self.space.elements() {
            for (PopupKind::Xdg(p), _) in
                PopupManager::popups_for_surface(w.toplevel().wl_surface())
            {
                if !self
                    .s_hovered_surface
                    .iter()
                    .any(|hs| &hs.surface == w.toplevel().wl_surface())
                {
                    p.send_popup_done();
                }
            }
        }
    }

    pub(crate) fn handle_focus(&mut self) {
        let (layer_surface, layer_shell_wl_surface) =
            if let Some(layer_surface) = self.layer.as_ref() {
                (layer_surface, layer_surface.wl_surface())
            } else {
                return;
            };
        let cur_focus = {
            let c_focused_surface = self.c_focused_surface.borrow();
            let c_hovered_surface = self.c_hovered_surface.borrow();
            // always visible if not configured for autohide
            if self.config.autohide().is_none() {
                return;
            }

            c_focused_surface
                .iter()
                .chain(c_hovered_surface.iter())
                .fold(
                    FocusStatus::LastFocused(self.start_instant),
                    |acc, (surface, _, f)| {
                        if self
                            .layer
                            .as_ref()
                            .map(|s| *s.wl_surface() == *surface)
                            .unwrap_or(false)
                            || self.popups.iter().any(|p| {
                                &p.c_wl_surface == surface
                                    || self.popups.iter().any(|p| p.c_wl_surface == *surface)
                            })
                        {
                            match (&acc, &f) {
                                (
                                    FocusStatus::LastFocused(t_acc),
                                    FocusStatus::LastFocused(t_cur),
                                ) => {
                                    if t_cur > t_acc {
                                        *f
                                    } else {
                                        acc
                                    }
                                }
                                (FocusStatus::LastFocused(_), FocusStatus::Focused) => *f,
                                _ => acc,
                            }
                        } else {
                            acc
                        }
                    },
                )
        };
        match self.visibility {
            Visibility::Hidden => {
                if let FocusStatus::Focused = cur_focus {
                    // start transition to visible
                    let margin = match self.config.anchor() {
                        PanelAnchor::Left | PanelAnchor::Right => -(self.dimensions.w),
                        PanelAnchor::Top | PanelAnchor::Bottom => -(self.dimensions.h),
                    } + self.config.get_hide_handle().unwrap() as i32;
                    self.visibility = Visibility::TransitionToVisible {
                        last_instant: Instant::now(),
                        progress: Duration::new(0, 0),
                        prev_margin: margin,
                    }
                }
            }
            Visibility::Visible => {
                if let FocusStatus::LastFocused(t) = cur_focus {
                    // start transition to hidden
                    let duration_since_last_focus = match Instant::now().checked_duration_since(t) {
                        Some(d) => d,
                        None => return,
                    };
                    if duration_since_last_focus > self.config.get_hide_wait().unwrap() {
                        self.visibility = Visibility::TransitionToHidden {
                            last_instant: Instant::now(),
                            progress: Duration::new(0, 0),
                            prev_margin: 0,
                        }
                    }
                }
            }
            Visibility::TransitionToHidden {
                last_instant,
                progress,
                prev_margin,
            } => {
                let now = Instant::now();
                let total_t = self.config.get_hide_transition().unwrap();
                let delta_t = match now.checked_duration_since(last_instant) {
                    Some(d) => d,
                    None => return,
                };
                let prev_progress = progress;
                let progress = match prev_progress.checked_add(delta_t) {
                    Some(d) => d,
                    None => return,
                };
                let progress_norm =
                    smootherstep(progress.as_millis() as f32 / total_t.as_millis() as f32);
                let handle = self.config.get_hide_handle().unwrap() as i32;

                if let FocusStatus::Focused = cur_focus {
                    // start transition to visible
                    self.visibility = Visibility::TransitionToVisible {
                        last_instant: now,
                        progress: total_t.checked_sub(progress).unwrap_or_default(),
                        prev_margin,
                    }
                } else {
                    let panel_size = match self.config.anchor() {
                        PanelAnchor::Left | PanelAnchor::Right => self.dimensions.w,
                        PanelAnchor::Top | PanelAnchor::Bottom => self.dimensions.h,
                    };
                    let target = -panel_size + handle;

                    let cur_pix = (progress_norm * target as f32) as i32;

                    if progress > total_t {
                        // XXX needs testing, but docs say that the margin value is only applied to anchored edge
                        if self.config.exclusive_zone() {
                            layer_surface.set_exclusive_zone(handle);
                        }
                        match self.config.anchor {
                            PanelAnchor::Left => layer_surface.set_margin(0, 0, 0, target),
                            PanelAnchor::Right => layer_surface.set_margin(0, target, 0, 0),
                            PanelAnchor::Top => layer_surface.set_margin(target, 0, 0, 0),
                            PanelAnchor::Bottom => layer_surface.set_margin(0, 0, target, 0),
                        };
                        layer_shell_wl_surface.commit();
                        self.visibility = Visibility::Hidden;
                    } else {
                        if prev_margin != cur_pix {
                            if self.config.exclusive_zone() {
                                layer_surface.set_exclusive_zone(panel_size - cur_pix);
                            }
                            match self.config.anchor {
                                PanelAnchor::Left => layer_surface.set_margin(0, 0, 0, cur_pix),
                                PanelAnchor::Right => layer_surface.set_margin(0, cur_pix, 0, 0),
                                PanelAnchor::Top => layer_surface.set_margin(cur_pix, 0, 0, 0),
                                PanelAnchor::Bottom => layer_surface.set_margin(0, 0, cur_pix, 0),
                            };
                            layer_shell_wl_surface.commit();
                        }
                        self.close_popups();
                        self.visibility = Visibility::TransitionToHidden {
                            last_instant: now,
                            progress,
                            prev_margin: cur_pix,
                        };
                    }
                }
            }
            Visibility::TransitionToVisible {
                last_instant,
                progress,
                prev_margin,
            } => {
                let now = Instant::now();
                let total_t = self.config.get_hide_transition().unwrap();
                let delta_t = match now.checked_duration_since(last_instant) {
                    Some(d) => d,
                    None => return,
                };
                let prev_progress = progress;
                let progress = match prev_progress.checked_add(delta_t) {
                    Some(d) => d,
                    None => return,
                };
                let progress_norm =
                    smootherstep(progress.as_millis() as f32 / total_t.as_millis() as f32);
                let handle = self.config.get_hide_handle().unwrap() as i32;

                if let FocusStatus::LastFocused(_) = cur_focus {
                    // start transition to visible
                    self.close_popups();
                    self.visibility = Visibility::TransitionToHidden {
                        last_instant: now,
                        progress: total_t.checked_sub(progress).unwrap_or_default(),
                        prev_margin,
                    }
                } else {
                    let panel_size = match self.config.anchor() {
                        PanelAnchor::Left | PanelAnchor::Right => self.dimensions.w,
                        PanelAnchor::Top | PanelAnchor::Bottom => self.dimensions.h,
                    };
                    let start = -panel_size + handle;

                    let cur_pix = ((1.0 - progress_norm) * start as f32) as i32;

                    if progress > total_t {
                        // XXX needs thorough testing, but docs say that the margin value is only applied to anchored edge
                        if self.config.exclusive_zone() {
                            layer_surface.set_exclusive_zone(panel_size);
                        }
                        layer_surface.set_margin(0, 0, 0, 0);
                        layer_shell_wl_surface.commit();
                        self.visibility = Visibility::Visible;
                    } else {
                        if prev_margin != cur_pix {
                            if self.config.exclusive_zone() {
                                layer_surface.set_exclusive_zone(panel_size - cur_pix);
                            }
                            match self.config.anchor {
                                PanelAnchor::Left => layer_surface.set_margin(0, 0, 0, cur_pix),
                                PanelAnchor::Right => layer_surface.set_margin(0, cur_pix, 0, 0),
                                PanelAnchor::Top => layer_surface.set_margin(cur_pix, 0, 0, 0),
                                PanelAnchor::Bottom => layer_surface.set_margin(0, 0, cur_pix, 0),
                            };
                            layer_shell_wl_surface.commit();
                        }
                        self.visibility = Visibility::TransitionToVisible {
                            last_instant: now,
                            progress,
                            prev_margin: cur_pix,
                        };
                    }
                }
            }
        }
    }

    pub(crate) fn constrain_dim(&self, size: Size<i32, Logical>) -> Size<i32, Logical> {
        let mut w = size.w.try_into().unwrap();
        let mut h = size.h.try_into().unwrap();

        let output_dims = self
            .output
            .as_ref()
            .and_then(|(_, _, info)| {
                info.modes
                    .iter()
                    .find_map(|m| if m.current { Some(m.dimensions) } else { None })
            })
            .map(|(w, h)| (w as u32, h as u32));

        if let (Some(w_range), _) = self
            .config
            .get_dimensions(output_dims, self.suggested_length)
        {
            if w < w_range.start {
                w = w_range.start;
            } else if w >= w_range.end {
                w = w_range.end - 1;
            }
        }
        if let (_, Some(h_range)) = self
            .config
            .get_dimensions(output_dims, self.suggested_length)
        {
            if h < h_range.start {
                h = h_range.start;
            } else if h >= h_range.end {
                h = h_range.end - 1;
            }
        }

        (w as i32, h as i32).into()
    }

    pub(crate) fn render(&mut self, renderer: &mut Gles2Renderer, time: u32) -> anyhow::Result<()> {
        if self.space_event.get() != None {
            return Ok(());
        }
        if self.is_dirty {
            let my_renderer = match self.damage_tracked_renderer.as_mut() {
                Some(r) => r,
                None => return Ok(()),
            };
            let _ = renderer.unbind();
            renderer.bind(self.egl_surface.as_ref().unwrap().clone())?;
            let clear_color = [0.0, 0.0, 0.0, 0.0];

            if let Some((o, _info)) = &self.output.as_ref().map(|(_, o, info)| (o, info)) {
                let elements: Vec<WaylandSurfaceRenderElement<_>> = self
                    .space
                    .elements()
                    .map(|w| {
                        let loc = self
                            .space
                            .element_location(w)
                            .unwrap_or_default()
                            .to_physical(1);
                        render_elements_from_surface_tree(
                            renderer,
                            w.toplevel().wl_surface(),
                            loc,
                            1.0,
                            self.log.clone(),
                        )
                    })
                    .flatten()
                    .collect_vec();

                let mut res = my_renderer
                    .render_output(
                        renderer,
                        self.egl_surface
                            .as_ref()
                            .unwrap()
                            .buffer_age()
                            .unwrap_or_default() as usize,
                        &elements,
                        self.bg_color,
                        self.log.clone(),
                    )
                    .unwrap();

                // TODO draw the rectangle properly
                let dock_rectangle: Rectangle<i32, Physical> = match self.config.anchor() {
                    PanelAnchor::Left | PanelAnchor::Right => Rectangle::from_loc_and_size(
                        (0, (self.dimensions.h - self.actual_length as i32) / 2),
                        (self.dimensions.w, self.actual_length as i32),
                    ),
                    PanelAnchor::Top | PanelAnchor::Bottom => Rectangle::from_loc_and_size(
                        ((self.dimensions.w - self.actual_length as i32) / 2, 0),
                        (self.actual_length as i32, self.dimensions.h),
                    ),
                };

                self.egl_surface
                    .as_ref()
                    .unwrap()
                    .swap_buffers(res.0.as_deref_mut())?;
                let _ = renderer.unbind();

                for window in self.space.elements() {
                    let output = o.clone();
                    window.send_frame(o, Duration::from_millis(time as u64), None, move |_, _| {
                        Some(output.clone())
                    });
                }
            }

            // TODO Popup rendering optimization
            for p in self
                .popups
                .iter_mut()
                .filter(|p| p.dirty && p.state.is_none())
            {
                let _ = renderer.unbind();
                renderer.bind(p.egl_surface.as_ref().unwrap().clone())?;

                let elements: Vec<WaylandSurfaceRenderElement<_>> =
                    render_elements_from_surface_tree(
                        renderer,
                        p.s_surface.wl_surface(),
                        (0, 0),
                        1.0,
                        self.log.clone(),
                    );
                if let Ok(mut frame) = renderer.render(
                    p.rectangle.size.to_physical(1),
                    smithay::utils::Transform::Flipped180,
                ) {
                    let _ = frame.clear(clear_color, &[p.rectangle.to_physical(1)]);
                    for element in elements {
                        let _ = element.draw(
                            &mut frame,
                            p.rectangle.loc.to_physical(1),
                            1.0.into(),
                            &[p.rectangle.to_physical(1)],
                            &self.log,
                        );
                    }
                    let _ = frame.finish();
                }

                p.egl_surface.as_ref().unwrap().swap_buffers(None)?;
                p.dirty = false;
                p.full_clear = p.full_clear.checked_sub(1).unwrap_or_default();
            }
        }

        self.is_dirty = false;
        self.full_clear = self.full_clear.checked_sub(1).unwrap_or_default();
        Ok(())
    }

    pub(crate) fn update_window_locations(&mut self) -> anyhow::Result<()> {
        let padding = self.config.padding();
        let anchor = self.config.anchor();
        let spacing = self.config.spacing();
        // First try partitioning the panel evenly into N spaces.
        // If all windows fit into each space, then set their offsets and return.
        let (list_length, list_thickness) = match anchor {
            PanelAnchor::Left | PanelAnchor::Right => (self.dimensions.h, self.dimensions.w),
            PanelAnchor::Top | PanelAnchor::Bottom => (self.dimensions.w, self.dimensions.h),
        };

        let mut num_lists = 0;
        if self.config.plugins_wings.is_some() {
            num_lists += 2;
        }
        let mut is_dock = false;
        if self.config.plugins_center.is_some() {
            if num_lists == 0 {
                is_dock = true;
            }
            num_lists += 1;
        }

        let mut windows_right = self
            .space
            .elements()
            .cloned()
            .filter_map(|w| {
                self.clients_right
                    .iter()
                    .enumerate()
                    .find_map(|(i, (_, c, _))| {
                        if Some(c.id()) == w.toplevel().wl_surface().client().map(|c| c.id()) {
                            Some((i, w.clone()))
                        } else {
                            None
                        }
                    })
            })
            .collect_vec();
        windows_right.sort_by(|(a_i, _), (b_i, _)| a_i.cmp(b_i));

        let mut windows_center = self
            .space
            .elements()
            .cloned()
            .filter_map(|w| {
                self.clients_center
                    .iter()
                    .enumerate()
                    .find_map(|(i, (_, c, _))| {
                        if Some(c.id()) == w.toplevel().wl_surface().client().map(|c| c.id()) {
                            Some((i, w.clone()))
                        } else {
                            None
                        }
                    })
            })
            .collect_vec();
        windows_center.sort_by(|(a_i, _), (b_i, _)| a_i.cmp(b_i));

        let mut windows_left = self
            .space
            .elements()
            .cloned()
            .filter_map(|w| {
                self.clients_left
                    .iter()
                    .enumerate()
                    .find_map(|(i, (_, c, _))| {
                        if Some(c.id()) == w.toplevel().wl_surface().client().map(|c| c.id()) {
                            Some((i, w.clone()))
                        } else {
                            None
                        }
                    })
            })
            .collect_vec();
        windows_left.sort_by(|(a_i, _), (b_i, _)| a_i.cmp(b_i));

        fn map_fn(
            (i, w): &(usize, Window),
            anchor: PanelAnchor,
            alignment: Alignment,
        ) -> (Alignment, usize, i32, i32) {
            match anchor {
                PanelAnchor::Left | PanelAnchor::Right => {
                    (alignment, *i, w.bbox().size.h, w.bbox().size.w)
                }
                PanelAnchor::Top | PanelAnchor::Bottom => {
                    (alignment, *i, w.bbox().size.w, w.bbox().size.h)
                }
            }
        }

        let left = windows_left
            .iter()
            .map(|e| map_fn(e, anchor, Alignment::Left));
        let left_sum = left.clone().map(|(_, _, length, _)| length).sum::<i32>()
            + spacing as i32 * (windows_left.len().max(1) as i32 - 1);

        let center = windows_center
            .iter()
            .map(|e| map_fn(e, anchor, Alignment::Center));
        let center_sum = center.clone().map(|(_, _, length, _)| length).sum::<i32>()
            + spacing as i32 * (windows_center.len().max(1) as i32 - 1);

        let right = windows_right
            .iter()
            .map(|e| map_fn(e, anchor, Alignment::Right));

        let right_sum = right.clone().map(|(_, _, length, _)| length).sum::<i32>()
            + spacing as i32 * (windows_right.len().max(1) as i32 - 1);

        // TODO should the center area in the panel be scrollable? and if there are too many on the sides the rightmost are moved to the center?
        let total_sum = left_sum + center_sum + right_sum;
        let new_list_length =
            total_sum + padding as i32 * 2 + spacing as i32 * (num_lists as i32 - 1);
        let new_list_thickness: i32 = 2 * padding as i32
            + chain!(left.clone(), center.clone(), right.clone())
                .map(|(_, _, _, thickness)| thickness)
                .max()
                .unwrap_or(0);
        let mut new_dim: Size<i32, Logical> = match anchor {
            PanelAnchor::Left | PanelAnchor::Right => (new_list_thickness, new_list_length),
            PanelAnchor::Top | PanelAnchor::Bottom => (new_list_length, new_list_thickness),
        }
        .into();

        // update input region of panel when list length changes
        if self.actual_length != new_list_length as u32 && !self.config.expand_to_edges {
            let (input_region, layer) = match (self.input_region.as_ref(), self.layer.as_ref()) {
                (Some(r), Some(layer)) => (r, layer),
                _ => anyhow::bail!("Missing input region or layer!"),
            };
            input_region.subtract(
                0,
                0,
                self.dimensions.w.max(new_dim.w),
                self.dimensions.h.max(new_dim.h),
            );

            let (layer_length, _) = if self.config.is_horizontal() {
                (self.dimensions.w, self.dimensions.h)
            } else {
                (self.dimensions.h, self.dimensions.w)
            };

            if new_list_length < layer_length {
                let side = (layer_length as u32 - new_list_length as u32) / 2;

                // clear center
                let loc = if self.config.is_horizontal() {
                    (side as i32, 0)
                } else {
                    (0, side as i32)
                };

                input_region.add(loc.0, loc.1, new_dim.w, new_dim.h);
            } else {
                input_region.add(
                    0,
                    0,
                    self.dimensions.w.max(new_dim.w),
                    self.dimensions.h.max(new_dim.h),
                );
            }
            layer
                .wl_surface()
                .set_input_region(Some(input_region.wl_region()));
            layer.wl_surface().commit();
        }

        self.actual_length = new_list_length as u32;
        new_dim = self.constrain_dim(new_dim);

        // new_dim.h = 400;
        let (new_list_length, new_list_thickness) = match anchor {
            PanelAnchor::Left | PanelAnchor::Right => (new_dim.h, new_dim.w),
            PanelAnchor::Top | PanelAnchor::Bottom => (new_dim.w, new_dim.h),
        };

        if new_list_length != list_length as i32 || new_list_thickness != list_thickness {
            self.pending_dimensions = Some(new_dim);
            self.full_clear = 4;
            anyhow::bail!("resizing list");
        }

        fn center_in_bar(thickness: u32, dim: u32) -> i32 {
            (thickness as i32 - dim as i32) / 2
        }

        let requested_eq_length: i32 = list_length / num_lists;
        let (right_sum, center_offset) = if is_dock {
            (0, padding as i32 + (list_length - center_sum) / 2)
        } else if left_sum <= requested_eq_length
            && center_sum <= requested_eq_length
            && right_sum <= requested_eq_length
        {
            let center_padding = (requested_eq_length - center_sum) / 2;
            (
                right_sum,
                requested_eq_length + padding as i32 + center_padding,
            )
        } else {
            let center_padding = (list_length as i32 - total_sum) / 2;

            (right_sum, left_sum + padding as i32 + center_padding)
        };

        let mut prev: u32 = padding;

        for (i, w) in &mut windows_left.iter_mut() {
            let size: Point<_, Logical> = (w.bbox().size.w, w.bbox().size.h).into();
            let cur: u32 = prev + spacing * *i as u32;
            match anchor {
                PanelAnchor::Left | PanelAnchor::Right => {
                    let cur = (
                        center_in_bar(list_thickness.try_into().unwrap(), size.x as u32),
                        cur,
                    );
                    prev += size.y as u32;
                    self.space
                        .map_element(w.clone(), (cur.0 as i32, cur.1 as i32), false);
                }
                PanelAnchor::Top | PanelAnchor::Bottom => {
                    let cur = (
                        cur,
                        center_in_bar(list_thickness.try_into().unwrap(), size.y as u32),
                    );
                    prev += size.x as u32;
                    self.space
                        .map_element(w.clone(), (cur.0 as i32, cur.1 as i32), false);
                }
            };
        }

        let mut prev: u32 = center_offset as u32;
        for (i, w) in &mut windows_center.iter_mut() {
            let size: Point<_, Logical> = (w.bbox().size.w, w.bbox().size.h).into();
            let cur = prev + spacing * *i as u32;
            match anchor {
                PanelAnchor::Left | PanelAnchor::Right => {
                    let cur = (
                        center_in_bar(list_thickness.try_into().unwrap(), size.x as u32),
                        cur,
                    );
                    prev += size.y as u32;
                    self.space
                        .map_element(w.clone(), (cur.0 as i32, cur.1 as i32), false);
                }
                PanelAnchor::Top | PanelAnchor::Bottom => {
                    let cur = (
                        cur,
                        center_in_bar(list_thickness.try_into().unwrap(), size.y as u32),
                    );
                    prev += size.x as u32;
                    self.space
                        .map_element(w.clone(), (cur.0 as i32, cur.1 as i32), false);
                }
            };
        }

        // twice padding is subtracted
        let mut prev: u32 = list_length as u32 - padding - right_sum as u32;

        for (i, w) in &mut windows_right.iter_mut() {
            let size: Point<_, Logical> = (w.bbox().size.w, w.bbox().size.h).into();
            let cur = prev + spacing * *i as u32;
            match anchor {
                PanelAnchor::Left | PanelAnchor::Right => {
                    let cur = (
                        center_in_bar(list_thickness.try_into().unwrap(), size.x as u32),
                        cur,
                    );
                    prev += size.y as u32;
                    self.space
                        .map_element(w.clone(), (cur.0 as i32, cur.1 as i32), false);
                }
                PanelAnchor::Top | PanelAnchor::Bottom => {
                    let cur = (
                        cur,
                        center_in_bar(list_thickness.try_into().unwrap(), size.y as u32),
                    );
                    prev += size.x as u32;
                    self.space
                        .map_element(w.clone(), (cur.0 as i32, cur.1 as i32), false);
                }
            };
        }
        self.space.refresh();

        Ok(())
    }

    pub(crate) fn handle_events(
        &mut self,
        dh: &DisplayHandle,
        popup_manager: &mut PopupManager,
        time: u32,
        renderer: Option<&mut Gles2Renderer>,
        egl_display: Option<&mut EGLDisplay>,
    ) -> Instant {
        self.space.refresh();
        popup_manager.cleanup();

        self.handle_focus();
        let mut should_render = false;
        match self.space_event.take() {
            Some(SpaceEvent::Quit) => {
                info!(self.log, "root layer shell surface removed.");
            }
            Some(SpaceEvent::WaitConfigure {
                first,
                width,
                height,
            }) => {
                self.space_event.replace(Some(SpaceEvent::WaitConfigure {
                    first,
                    width,
                    height,
                }));
            }
            None => {
                if let (Some(size), Some(layer_surface)) =
                    (self.pending_dimensions.take(), self.layer.as_ref())
                {
                    let width = size.w.try_into().unwrap();
                    let height = size.h.try_into().unwrap();
                    if self.config.is_horizontal() {
                        layer_surface.set_size(0, height);
                    } else {
                        layer_surface.set_size(width, 0);
                    }
                    if self.config().autohide.is_some() {
                        if self.config.exclusive_zone() {
                            self.layer
                                .as_ref()
                                .unwrap()
                                .set_exclusive_zone(self.config.get_hide_handle().unwrap() as i32);
                        } else {
                            self.layer.as_ref().unwrap().set_exclusive_zone(-1);
                        }
                        let target = match (&self.visibility, self.config.anchor()) {
                            (Visibility::Hidden, PanelAnchor::Left | PanelAnchor::Right) => -size.w,
                            (Visibility::Hidden, PanelAnchor::Top | PanelAnchor::Bottom) => -size.h,
                            _ => 0,
                        } + self.config.get_hide_handle().unwrap() as i32;
                        match self.config.anchor {
                            PanelAnchor::Left => layer_surface.set_margin(0, 0, 0, target),
                            PanelAnchor::Right => layer_surface.set_margin(0, target, 0, 0),
                            PanelAnchor::Top => layer_surface.set_margin(target, 0, 0, 0),
                            PanelAnchor::Bottom => layer_surface.set_margin(0, 0, target, 0),
                        };
                    } else if self.config.exclusive_zone() {
                        let list_thickness = match self.config.anchor() {
                            PanelAnchor::Left | PanelAnchor::Right => width,
                            PanelAnchor::Top | PanelAnchor::Bottom => height,
                        };
                        self.layer
                            .as_ref()
                            .unwrap()
                            .set_exclusive_zone(list_thickness as i32);
                    }
                    layer_surface.wl_surface().commit();
                    self.space_event.replace(Some(SpaceEvent::WaitConfigure {
                        first: false,
                        width: size.w,
                        height: size.h,
                    }));
                } else if self.layer.is_some() {
                    should_render = if self.full_clear == 4 {
                        let update_res = self.update_window_locations();
                        update_res.is_ok()
                    } else {
                        true
                    };
                }
            }
        }

        if let (Some(renderer), Some(egl_display)) = (renderer, egl_display) {
            self.popups.retain_mut(|p: &mut WrapperPopup| {
                p.handle_events(
                    popup_manager,
                    renderer.egl_context(),
                    egl_display,
                    self.c_display.as_ref().unwrap(),
                )
            });

            if should_render {
                let _ = self.render(renderer, time);
            }
            if let Some(egl_surface) = self.egl_surface.as_ref() {
                if egl_surface.get_size() != Some(self.dimensions.to_physical(1)) {
                    self.full_clear = 4;
                }
            }
        }

        self.last_dirty.unwrap_or_else(Instant::now)
    }

    pub fn configure_panel_layer(
        &mut self,
        layer: &LayerSurface,
        configure: sctk::shell::layer::LayerSurfaceConfigure,
        renderer: &mut Option<Gles2Renderer>,
        egl_display: &mut Option<EGLDisplay>,
    ) {
        let (w, h) = configure.new_size;
        match self.space_event.take() {
            Some(e) => match e {
                SpaceEvent::WaitConfigure {
                    first,
                    mut width,
                    mut height,
                } => {
                    let _ = self.spawn_clients(self.s_display.clone().unwrap());
                    if w != 0 {
                        width = w as i32;
                        if self.config.is_horizontal() {
                            self.suggested_length.replace(w);
                        }
                    }
                    if h != 0 {
                        height = h as i32;
                        if !self.config.is_horizontal() {
                            self.suggested_length.replace(h);
                        }
                    }
                    if width == 0 {
                        width = 1;
                    }
                    if height == 0 {
                        height = 1;
                    }
                    if first {
                        let log = self.log.clone();
                        let client_egl_surface = unsafe {
                            ClientEglSurface::new(
                                WlEglSurface::new(
                                    self.layer.as_ref().unwrap().wl_surface().id(),
                                    width,
                                    height,
                                )
                                .unwrap(), // TODO remove unwrap
                                self.c_display.as_ref().unwrap().clone(),
                                self.layer.as_ref().unwrap().wl_surface().clone(),
                            )
                        };
                        let new_egl_display = if let Some(egl_display) = egl_display.take() {
                            egl_display
                        } else {
                            unsafe { EGLDisplay::new(&client_egl_surface, log.clone()) }
                                .expect("Failed to create EGL display")
                        };

                        let egl_context = EGLContext::new_with_config(
                            &new_egl_display,
                            GlAttributes {
                                version: (3, 0),
                                profile: None,
                                debug: cfg!(debug_assertions),
                                vsync: false,
                            },
                            Default::default(),
                            log.clone(),
                        )
                        .unwrap_or_else(|_| {
                            EGLContext::new_with_config(
                                &new_egl_display,
                                GlAttributes {
                                    version: (2, 0),
                                    profile: None,
                                    debug: cfg!(debug_assertions),
                                    vsync: false,
                                },
                                Default::default(),
                                log.clone(),
                            )
                            .expect("Failed to create EGL context")
                        });

                        let new_renderer = if let Some(renderer) = renderer.take() {
                            renderer
                        } else {
                            unsafe {
                                Gles2Renderer::new(egl_context, log.clone())
                                    .expect("Failed to create EGL Surface")
                            }
                        };

                        let egl_surface = Rc::new(
                            EGLSurface::new(
                                &new_egl_display,
                                new_renderer
                                    .egl_context()
                                    .pixel_format()
                                    .expect("Failed to get pixel format from EGL context "),
                                new_renderer.egl_context().config_id(),
                                client_egl_surface,
                                log.clone(),
                            )
                            .expect("Failed to create EGL Surface"),
                        );

                        renderer.replace(new_renderer);
                        egl_display.replace(new_egl_display);
                        self.egl_surface.replace(egl_surface);
                    } else if self.dimensions != (width as i32, height as i32).into()
                        && self.pending_dimensions.is_none()
                    {
                        self.egl_surface.as_ref().unwrap().resize(
                            width as i32,
                            height as i32,
                            0,
                            0,
                        );
                    }
                    self.dimensions = (w as i32, h as i32).into();
                    self.damage_tracked_renderer
                        .replace(DamageTrackedRenderer::new(
                            self.dimensions.to_physical(1),
                            1.0,
                            smithay::utils::Transform::Flipped180,
                        ));
                    self.layer.as_ref().unwrap().wl_surface().commit();
                    self.full_clear = 4;
                }
                SpaceEvent::Quit => (),
            },
            None => {
                let mut width = self.dimensions.w;
                let mut height = self.dimensions.h;
                if w != 0 {
                    width = w as i32;
                    if self.config.is_horizontal() {
                        self.suggested_length.replace(w);
                    }
                }
                if h != 0 {
                    height = h as i32;
                    if !self.config.is_horizontal() {
                        self.suggested_length.replace(h);
                    }
                }
                if width == 0 {
                    width = 1;
                }
                if height == 0 {
                    height = 1;
                }

                self.egl_surface
                    .as_ref()
                    .unwrap()
                    .resize(width as i32, height as i32, 0, 0);
                self.dimensions = (width as i32, height as i32).into();
                self.damage_tracked_renderer
                    .replace(DamageTrackedRenderer::new(
                        self.dimensions.to_physical(1),
                        1.0,
                        smithay::utils::Transform::Flipped180,
                    ));
                self.layer.as_ref().unwrap().wl_surface().commit();
                // self.w_accumulated_damage.clear();
                self.full_clear = 4;
            }
        }
    }

    pub fn configure_panel_popup(
        &mut self,
        popup: &sctk::shell::xdg::popup::Popup,
        config: sctk::shell::xdg::popup::PopupConfigure,
        renderer: Option<&mut Gles2Renderer>,
        egl_display: Option<&mut EGLDisplay>,
    ) {
        let (renderer, egl_display) =
            if let (Some(renderer), Some(egl_display)) = (renderer, egl_display) {
                (renderer, egl_display)
            } else {
                return;
            };

        let pos = config.position;
        let (width, height) = (config.width, config.height);
        if let Some(p) = self
            .popups
            .iter_mut()
            .find(|p| popup.wl_surface() == p.c_popup.wl_surface())
        {
            p.wrapper_rectangle = Rectangle::from_loc_and_size(pos, (width, height));
            p.state.take();
            let _ = p.s_surface.send_configure();
            match config.kind {
                popup::ConfigureKind::Initial => {
                    let wl_egl_surface = match WlEglSurface::new(p.c_wl_surface.id(), width, height)
                    {
                        Ok(s) => s,
                        Err(_) => return,
                    };
                    let client_egl_surface = unsafe {
                        ClientEglSurface::new(
                            wl_egl_surface,
                            self.c_display.as_ref().unwrap().clone(),
                            p.c_wl_surface.clone(),
                        )
                    };
                    let egl_context = renderer.egl_context();
                    let egl_surface = Rc::new(
                        EGLSurface::new(
                            egl_display,
                            egl_context
                                .pixel_format()
                                .expect("Failed to get pixel format from EGL context "),
                            egl_context.config_id(),
                            client_egl_surface,
                            None,
                        )
                        .expect("Failed to initialize EGL Surface"),
                    );
                    p.egl_surface.replace(egl_surface);
                }
                popup::ConfigureKind::Reactive => {
                    // TODO
                }
                popup::ConfigureKind::Reposition { token: _token } => {
                    // TODO
                }
                _ => {}
            };
            p.dirty = true;
            p.full_clear = 4;
        }
    }

    pub fn set_theme_window_color(&mut self, mut color: [f32; 4]) {
        if let CosmicPanelBackground::ThemeDefault(alpha) = self.config.background {
            if let Some(alpha) = alpha {
                color[3] = alpha;
            }
            self.bg_color = color;
        }
        self.bg_color = color;
        self.full_clear = 4;
    }
}
