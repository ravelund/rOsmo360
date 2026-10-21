//! The divider between the view and the transport row: dragged, it shares
//! the window's height between the picture and the timeline's lanes
//! (`timeline::Rows`); a double-click puts them back as they were.

use super::Message;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path};
use iced::{Point, Rectangle, Renderer, Size, Theme};
use std::time::{Duration, Instant};

/// Its height: enough to grab without hunting for it.
pub const HEIGHT: f32 = 7.0;
/// Two presses this close together are a double-click.
const DOUBLE: Duration = Duration::from_millis(400);

pub struct Divider;

#[derive(Default)]
pub struct State {
    /// Where the pointer was pressed, while dragging: each move reports
    /// how far it has gone since, so past a limit the divider stays put
    /// until the pointer comes back to it.
    from: Option<f32>,
    last_press: Option<Instant>,
}

impl canvas::Program<Message> for Divider {
    type State = State;

    fn update(&self, state: &mut State, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<canvas::Action<Message>> {
        match event {
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cursor.position_over(bounds)?;
                let now = Instant::now();
                if state.last_press.is_some_and(|t| now - t < DOUBLE) {
                    state.last_press = None;
                    return Some(canvas::Action::publish(Message::SplitReset).and_capture());
                }
                state.last_press = Some(now);
                state.from = Some(p.y);
                Some(canvas::Action::request_redraw().and_capture())
            }
            // Followed outside its few pixels too: the pointer runs ahead of
            // the redraw.
            iced::Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let from = state.from?;
                Some(canvas::Action::publish(Message::SplitDrag(position.y - from)).and_capture())
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.from.take()?;
                Some(canvas::Action::publish(Message::SplitDone).and_capture())
            }
            _ => None,
        }
    }

    fn draw(&self, state: &State, renderer: &Renderer, theme: &Theme, bounds: Rectangle, cursor: mouse::Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let palette = theme.extended_palette();
        let lit = state.from.is_some() || cursor.is_over(bounds);
        let colour = if lit { palette.primary.strong.color } else { palette.background.strong.color };
        // A short grip in the middle says it can be taken hold of.
        let w = 48.0;
        frame.fill(&Path::rounded_rectangle(Point::new((bounds.width - w) / 2.0, (HEIGHT - 3.0) / 2.0), Size::new(w, 3.0), 1.5.into()), colour);
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, state: &State, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        if state.from.is_some() || cursor.is_over(bounds) {
            mouse::Interaction::ResizingVertically
        } else {
            mouse::Interaction::default()
        }
    }
}
