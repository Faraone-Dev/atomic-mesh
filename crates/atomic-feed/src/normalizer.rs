use atomic_core::event::{EventPayload, OrderBookUpdateEvent, TradeEvent};
use atomic_core::types::{Level, Price, Qty, Side, Symbol, Venue};
use crate::connector::RawFeedMessage;

/// Normalizes raw exchange messages into atomic-core event payloads.
/// This ensures all downstream processing is exchange-agnostic.
pub struct FeedNormalizer {
    venue: Venue,
    price_decimals: u8,
    qty_decimals: u8,
}

impl FeedNormalizer {
    pub fn new(venue: Venue, price_decimals: u8, qty_decimals: u8) -> Self {
        Self { venue, price_decimals, qty_decimals }
    }

    pub fn normalize(&self, msg: RawFeedMessage) -> Option<EventPayload> {
        match msg {
            RawFeedMessage::OrderBookSnapshot { symbol, bids, asks, timestamp } => {
                let sym = self.parse_symbol(&symbol);
                Some(EventPayload::OrderBookUpdate(OrderBookUpdateEvent {
                    symbol: sym,
                    bids: self.convert_levels(&bids),
                    asks: self.convert_levels(&asks),
                    is_snapshot: true,
                    exchange_ts: timestamp,
                }))
            }
            RawFeedMessage::OrderBookDelta { symbol, bids, asks, timestamp } => {
                let sym = self.parse_symbol(&symbol);
                Some(EventPayload::OrderBookUpdate(OrderBookUpdateEvent {
                    symbol: sym,
                    bids: self.convert_levels(&bids),
                    asks: self.convert_levels(&asks),
                    is_snapshot: false,
                    exchange_ts: timestamp,
                }))
            }
            RawFeedMessage::Trade { symbol, price, qty, is_buy, timestamp } => {
                let sym = self.parse_symbol(&symbol);
                Some(EventPayload::Trade(TradeEvent {
                    symbol: sym,
                    price: Price::from_f64(price, self.price_decimals),
                    qty: Qty::from_f64(qty, self.qty_decimals),
                    side: if is_buy { Side::Buy } else { Side::Sell },
                    exchange_ts: timestamp,
                }))
            }
        }
    }

    fn convert_levels(&self, raw: &[(f64, f64)]) -> Vec<Level> {
        raw.iter()
            .map(|(p, q)| Level {
                price: Price::from_f64(*p, self.price_decimals),
                qty: Qty::from_f64(*q, self.qty_decimals),
            })
            .collect()
    }

    fn parse_symbol(&self, raw: &str) -> Symbol {
        // Common exchange formats: BTCUSDT, BTC/USDT, BTC-USDT
        let clean = raw.replace('-', "").replace('/', "");
        if clean.ends_with("USDT") {
            let base = &clean[..clean.len() - 4];
            Symbol::new(base, "USDT", self.venue)
        } else if clean.ends_with("USD") {
            let base = &clean[..clean.len() - 3];
            Symbol::new(base, "USD", self.venue)
        } else if clean.ends_with("BTC") {
            let base = &clean[..clean.len() - 3];
            Symbol::new(base, "BTC", self.venue)
        } else {
            Symbol::new(&clean, "UNKNOWN", self.venue)
        }
    }
}
