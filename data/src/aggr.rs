pub mod reaggregate;
pub mod ticks;
pub mod time;

use exchange::unit::Qty;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickCount(pub u16);

impl TickCount {
    pub const ALL: [TickCount; 7] = [
        TickCount(10),
        TickCount(20),
        TickCount(50),
        TickCount(100),
        TickCount(200),
        TickCount(500),
        TickCount(1000),
    ];

    pub fn is_custom(&self) -> bool {
        !Self::ALL.contains(self)
    }
}

impl std::fmt::Display for TickCount {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}T", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeThreshold(pub u32);

impl VolumeThreshold {
    pub const LOT_SIZE: u32 = 100;
    pub const ALL: [VolumeThreshold; 7] = [
        VolumeThreshold(100),
        VolumeThreshold(500),
        VolumeThreshold(1_000),
        VolumeThreshold(2_000),
        VolumeThreshold(5_000),
        VolumeThreshold(10_000),
        VolumeThreshold(50_000),
    ];

    pub fn is_custom(&self) -> bool {
        !Self::ALL.contains(self)
    }

    pub fn raw_qty_units(self) -> u64 {
        u64::from(self.0) * u64::from(Self::LOT_SIZE)
    }

    pub fn raw_qty(self) -> Qty {
        Qty::from_units((self.raw_qty_units() as i64) * 10i64.pow(Qty::QTY_SCALE as u32))
    }
}

impl std::fmt::Display for VolumeThreshold {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}V", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeSessionSplit {
    None,
    ChinaTradingDay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeAggregation {
    Tick(TickCount),
    Volume(VolumeThreshold),
}

impl TradeAggregation {
    pub fn x_axis_step(self) -> u64 {
        match self {
            Self::Tick(count) => u64::from(count.0),
            Self::Volume(threshold) => u64::from(threshold.0),
        }
    }
}
