use std::{sync::Mutex, time::Instant};

use cosmic_comp_config::ZoomMovement;
use keyframe::{ease, functions::EaseInOutCubic};
use smithay::{
    input::Seat,
    output::Output,
    utils::{Point, Rectangle, Size},
};

use crate::{
    state::State,
    utils::{
        prelude::*,
        tween::EasePoint,
    },
};

use super::ANIMATION_DURATION;

#[derive(Debug, Clone)]
pub struct ZoomState {
    pub(super) seat: Seat<State>,
    pub(super) show_overlay: bool,
    pub(super) increment: u32,
    pub(super) movement: ZoomMovement,
}

#[derive(Debug)]
pub struct OutputZoomState {
    pub(super) level: f64,
    pub(super) previous_level: Option<(f64, Instant)>,
    focal_point: Point<f64, Local>,
    previous_point: Option<(Point<f64, Local>, Instant)>,
}

impl OutputZoomState {
    pub fn new(
        seat: &Seat<State>,
        output: &Output,
        level: f64,
        _increment: u32,
        movement: ZoomMovement,
    ) -> OutputZoomState {
        let cursor_position = seat.get_pointer().unwrap().current_location().as_global();
        let output_geometry = output.geometry().to_f64();
        let focal_point = if output_geometry.contains(cursor_position) {
            match movement {
                ZoomMovement::Continuously | ZoomMovement::OnEdge => {
                    cursor_position.to_local(output)
                }
                ZoomMovement::Centered => {
                    let mut zoomed_output_geometry = output.geometry().to_f64().downscale(level);
                    zoomed_output_geometry.loc =
                        cursor_position - zoomed_output_geometry.size.downscale(2.).to_point();

                    let mut focal_point = zoomed_output_geometry
                        .loc
                        .to_local(output)
                        .upscale(level)
                        .to_global(output);
                    focal_point.x = focal_point.x.clamp(
                        output_geometry.loc.x,
                        (output_geometry.loc.x + output_geometry.size.w).next_down(),
                    );
                    focal_point.y = focal_point.y.clamp(
                        output_geometry.loc.y,
                        (output_geometry.loc.y + output_geometry.size.h).next_down(),
                    );
                    focal_point.to_local(output)
                }
            }
        } else {
            (output_geometry.size.w / 2., output_geometry.size.h / 2.).into()
        };

        OutputZoomState {
            level,
            previous_level: None,
            focal_point,
            previous_point: None,
        }
    }

    pub fn animating_focal_point(&mut self) -> Point<f64, Local> {
        if let Some((old_point, start)) = self.previous_point.as_ref() {
            let duration_since = Instant::now().duration_since(*start);
            if duration_since > ANIMATION_DURATION {
                self.previous_point.take();
                return self.focal_point;
            }

            let percentage =
                duration_since.as_millis() as f32 / ANIMATION_DURATION.as_millis() as f32;
            ease(
                EaseInOutCubic,
                EasePoint(*old_point),
                EasePoint(self.focal_point),
                percentage,
            )
            .0
        } else {
            self.focal_point
        }
    }

    pub fn current_focal_point(&mut self) -> Point<f64, Local> {
        self.focal_point
    }

    pub fn current_level(&self) -> f64 {
        self.level
    }

    pub fn animating_level(&self) -> f64 {
        if let Some((old_level, start)) = self.previous_level.as_ref() {
            let percentage = Instant::now().duration_since(*start).as_millis() as f32
                / ANIMATION_DURATION.as_millis() as f32;

            ease(EaseInOutCubic, *old_level, self.level, percentage)
        } else {
            self.level
        }
    }

    pub fn is_animating(&self) -> bool {
        self.previous_point.is_some() || self.previous_level.is_some()
    }

    pub fn refresh(&mut self) -> bool {
        if self
            .previous_level
            .as_ref()
            .is_some_and(|(_, start)| Instant::now().duration_since(*start) > ANIMATION_DURATION)
        {
            self.previous_level.take();
        }
        self.level == 1. && self.previous_level.is_none()
    }

    pub fn update(&mut self, level: f64, animate: bool, movement: ZoomMovement, _increment: u32) {
        self.previous_level = animate.then_some((self.animating_level(), Instant::now()));
        self.level = level;
    }
}

impl ZoomState {
    pub fn current_seat(&self) -> Seat<State> {
        self.seat.clone()
    }

    pub fn current_level(&self, output: &Output) -> f64 {
        let output_state = output.user_data().get::<Mutex<OutputZoomState>>().unwrap();
        output_state.lock().unwrap().current_level()
    }

    pub fn animating_level(&self, output: &Output) -> f64 {
        let output_state = output.user_data().get::<Mutex<OutputZoomState>>().unwrap();
        output_state.lock().unwrap().animating_level()
    }

    pub fn animating_focal_point(&self, output: Option<&Output>) -> Point<f64, Global> {
        let active_output = self.seat.active_output();
        let output = output.unwrap_or(&active_output);
        let output_state = output.user_data().get::<Mutex<OutputZoomState>>().unwrap();

        output_state
            .lock()
            .unwrap()
            .animating_focal_point()
            .to_global(output)
    }

    pub fn current_focal_point(&self, output: Option<&Output>) -> Point<f64, Global> {
        let active_output = self.seat.active_output();
        let output = output.unwrap_or(&active_output);
        let output_state = output.user_data().get::<Mutex<OutputZoomState>>().unwrap();

        output_state
            .lock()
            .unwrap()
            .current_focal_point()
            .to_global(output)
    }

    pub fn update_focal_point(
        &mut self,
        output: &Output,
        cursor_position: Point<f64, Global>,
        original_position: Point<f64, Global>,
        movement: ZoomMovement,
    ) {
        let cursor_position = cursor_position.to_i32_round();
        let original_position = original_position.to_i32_round();
        let output_geometry = output.geometry();
        let mut zoomed_output_geometry = output.zoomed_geometry().unwrap();

        let output_state = output.user_data().get::<Mutex<OutputZoomState>>().unwrap();
        let mut output_state_ref = output_state.lock().unwrap();

        if self.movement != movement {
            output_state_ref.previous_point = Some((output_state_ref.focal_point, Instant::now()));
            self.movement = movement;
        }

        let cursor_position = cursor_position.to_local(output);
        match movement {
            ZoomMovement::Continuously => output_state_ref.focal_point = cursor_position.to_f64(),
            ZoomMovement::OnEdge => {
                if !zoomed_output_geometry
                    .overlaps_or_touches(Rectangle::new(original_position, Size::from((16, 16))))
                {
                    zoomed_output_geometry.loc = cursor_position.to_global(output)
                        - zoomed_output_geometry.size.downscale(2).to_point();
                    let mut focal_point = zoomed_output_geometry
                        .loc
                        .to_local(output)
                        .upscale(
                            output_geometry.size.w
                                / (output_geometry.size.w - zoomed_output_geometry.size.w),
                        )
                        .to_global(output);
                    focal_point.x = focal_point.x.clamp(
                        output_geometry.loc.x,
                        output_geometry.loc.x + output_geometry.size.w - 1,
                    );
                    focal_point.y = focal_point.y.clamp(
                        output_geometry.loc.y,
                        output_geometry.loc.y + output_geometry.size.h - 1,
                    );
                    output_state_ref.previous_point =
                        Some((output_state_ref.focal_point, Instant::now()));
                    output_state_ref.focal_point = focal_point.to_local(output).to_f64();
                } else if !zoomed_output_geometry.contains(cursor_position.to_global(output)) {
                    let mut diff = output_state_ref.focal_point.to_global(output)
                        + (cursor_position.to_global(output) - original_position)
                            .to_f64()
                            .upscale(output_state_ref.level);
                    diff.x = diff.x.clamp(
                        output_geometry.loc.x as f64,
                        ((output_geometry.loc.x + output_geometry.size.w) as f64).next_down(),
                    );
                    diff.y = diff.y.clamp(
                        output_geometry.loc.y as f64,
                        ((output_geometry.loc.y + output_geometry.size.h) as f64).next_down(),
                    );
                    diff -= output_state_ref.focal_point.to_global(output);

                    output_state_ref.focal_point += diff.as_logical().as_local();
                }
            }
            ZoomMovement::Centered => {
                zoomed_output_geometry.loc = cursor_position.to_global(output)
                    - zoomed_output_geometry.size.downscale(2).to_point();

                let mut focal_point = zoomed_output_geometry
                    .loc
                    .to_local(output)
                    .upscale(
                        output_geometry
                            .size
                            .w
                            .checked_div(output_geometry.size.w - zoomed_output_geometry.size.w)
                            .unwrap_or(1),
                    )
                    .to_global(output);
                focal_point.x = focal_point.x.clamp(
                    output_geometry.loc.x,
                    output_geometry.loc.x + output_geometry.size.w - 1,
                );
                focal_point.y = focal_point.y.clamp(
                    output_geometry.loc.y,
                    output_geometry.loc.y + output_geometry.size.h - 1,
                );
                output_state_ref.focal_point = focal_point.to_local(output).to_f64();
            }
        }
    }

    /// The zoom UI no longer has an in-compositor surface. Returns None always.
    pub fn surface_under(
        &self,
        _output: &Output,
        _pos: Point<f64, Global>,
    ) -> Option<(super::focus::target::PointerFocusTarget, Point<f64, Global>)> {
        None
    }
}
