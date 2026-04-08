pub use dashboard::Dashboard;
pub use pane::Pane;
use serde::{Deserialize, Serialize};

pub mod dashboard;
pub mod pane;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    pub name: String,
    pub dashboard: Dashboard,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            name: "Default".to_string(),
            dashboard: Dashboard::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Window<T = f32> {
    pub width: T,
    pub height: T,
    pub pos_x: T,
    pub pos_y: T,
}

impl<T: Copy> Window<T> {
    pub fn size(&self) -> iced_core::Size<T> {
        iced_core::Size {
            width: self.width,
            height: self.height,
        }
    }

    pub fn position(&self) -> iced_core::Point<T> {
        iced_core::Point {
            x: self.pos_x,
            y: self.pos_y,
        }
    }
}

impl Default for Window<f32> {
    fn default() -> Self {
        Self {
            width: 1024.0,
            height: 768.0,
            pos_x: 0.0,
            pos_y: 0.0,
        }
    }
}

pub type WindowSpec = Window<f32>;

impl From<(&iced_core::Point, &iced_core::Size)> for WindowSpec {
    fn from((point, size): (&iced_core::Point, &iced_core::Size)) -> Self {
        Self {
            width: size.width,
            height: size.height,
            pos_x: point.x,
            pos_y: point.y,
        }
    }
}

impl WindowSpec {
    const MIN_VALID_DIMENSION: f32 = 64.0;
    const MINIMIZED_SENTINEL_THRESHOLD: f32 = -30_000.0;

    pub fn is_restore_safe(&self) -> bool {
        self.width.is_finite()
            && self.height.is_finite()
            && self.pos_x.is_finite()
            && self.pos_y.is_finite()
            && self.width >= Self::MIN_VALID_DIMENSION
            && self.height >= Self::MIN_VALID_DIMENSION
            && self.pos_x > Self::MINIMIZED_SENTINEL_THRESHOLD
            && self.pos_y > Self::MINIMIZED_SENTINEL_THRESHOLD
    }
}
