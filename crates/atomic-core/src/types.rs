use serde::{Deserialize, Serialize};
use std::fmt;

// === VENUE (exchange) ===

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Venue {
    Binance,
    Bybit,
    OKX,
    Deribit,
    Coinbase,
    Kraken,
    DYDX,
    Hyperliquid,
    Simulated,
}

impl fmt::Display for Venue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binance => write!(f, "binance"),
            Self::Bybit => write!(f, "bybit"),
            Self::OKX => write!(f, "okx"),
            Self::Deribit => write!(f, "deribit"),
            Self::Coinbase => write!(f, "coinbase"),
            Self::Kraken => write!(f, "kraken"),
            Self::DYDX => write!(f, "dydx"),
            Self::Hyperliquid => write!(f, "hyperliquid"),
            Self::Simulated => write!(f, "simulated"),
        }
    }
}

// === SIDE ===

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

// === ORDER TYPE ===

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit,
    StopMarket,
    StopLimit,
}

// === TIME IN FORCE ===

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimeInForce {
    GoodTilCancel,
    ImmediateOrCancel,
    FillOrKill,
    GoodTilDate(u64),
}

// === PRICE — integer pipettes, no floating point ===

/// Price in pipettes (1 pipette = smallest price increment).
/// For BTC/USDT with 0.01 tick: 60000.00 = 6_000_000 pipettes.
/// All arithmetic is integer — no float anywhere in the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Price(pub i64);

impl Price {
    pub const ZERO: Self = Self(0);

    pub fn from_f64(value: f64, tick_decimals: u8) -> Self {
        let multiplier = 10i64.pow(tick_decimals as u32);
        Self((value * multiplier as f64).round() as i64)
    }

    pub fn to_f64(self, tick_decimals: u8) -> f64 {
        let multiplier = 10f64.powi(tick_decimals as i32);
        self.0 as f64 / multiplier
    }

    pub fn midpoint(self, other: Self) -> Self {
        Self((self.0 + other.0) / 2)
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// === QUANTITY — integer, no floating point ===

/// Quantity in base units (satoshis for BTC, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Qty(pub u64);

impl Qty {
    pub const ZERO: Self = Self(0);

    pub fn from_f64(value: f64, decimals: u8) -> Self {
        let multiplier = 10u64.pow(decimals as u32);
        Self((value * multiplier as f64).round() as u64)
    }

    pub fn to_f64(self, decimals: u8) -> f64 {
        let multiplier = 10f64.powi(decimals as i32);
        self.0 as f64 / multiplier
    }

    pub fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

impl fmt::Display for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// === SYMBOL ===

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Symbol {
    pub base: String,
    pub quote: String,
    pub venue: Venue,
}

impl Symbol {
    pub fn new(base: &str, quote: &str, venue: Venue) -> Self {
        Self {
            base: base.to_uppercase(),
            quote: quote.to_uppercase(),
            venue,
        }
    }

    pub fn pair(&self) -> String {
        format!("{}/{}", self.base, self.quote)
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}/{}", self.venue, self.base, self.quote)
    }
}

// === ORDER ID ===

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrderId(pub String);

impl OrderId {
    pub fn new(venue: Venue, local_id: u64) -> Self {
        Self(format!("{}-{}", venue, local_id))
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// === FILL ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub order_id: OrderId,
    pub symbol: Symbol,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    pub fee: i64,
    pub timestamp: u64,
    pub venue: Venue,
    pub is_maker: bool,
}

// === POSITION ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub symbol: Symbol,
    pub side: Side,
    pub qty: Qty,
    pub avg_entry: Price,
    pub unrealized_pnl: i64,
    pub realized_pnl: i64,
}

impl Position {
    pub fn flat(symbol: Symbol) -> Self {
        Self {
            symbol,
            side: Side::Buy,
            qty: Qty::ZERO,
            avg_entry: Price::ZERO,
            unrealized_pnl: 0,
            realized_pnl: 0,
        }
    }

    pub fn is_flat(&self) -> bool {
        self.qty.0 == 0
    }

    pub fn notional(&self) -> i64 {
        self.qty.0 as i64 * self.avg_entry.0
    }
}

// === LEVEL (orderbook) ===

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Level {
    pub price: Price,
    pub qty: Qty,
}

// === ORDERBOOK SNAPSHOT ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookSnapshot {
    pub symbol: Symbol,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub timestamp: u64,
    pub seq: u64,
}

impl BookSnapshot {
    pub fn best_bid(&self) -> Option<Level> {
        self.bids.first().copied()
    }

    pub fn best_ask(&self) -> Option<Level> {
        self.asks.first().copied()
    }

    pub fn mid_price(&self) -> Option<Price> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some(bid.price.midpoint(ask.price)),
            _ => None,
        }
    }

    pub fn spread(&self) -> Option<i64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some(ask.price.0 - bid.price.0),
            _ => None,
        }
    }
}
