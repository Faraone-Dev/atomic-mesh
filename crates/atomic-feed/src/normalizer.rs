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

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::event::EventPayload;
    use atomic_core::types::{Price, Qty, Venue};

    #[test]
    fn normalize_trade_converts_float_to_integer() {
        let norm = FeedNormalizer::new(Venue::Binance, 2, 8);
        let msg = RawFeedMessage::Trade {
            symbol: "BTCUSDT".to_string(),
            price: 50000.50,
            qty: 0.123,
            is_buy: true,
            timestamp: 1000,
        };
        let payload = norm.normalize(msg).unwrap();
        match payload {
            EventPayload::Trade(t) => {
                assert_eq!(t.price, Price::from_f64(50000.50, 2));
                assert_eq!(t.qty, Qty::from_f64(0.123, 8));
                assert_eq!(t.side, Side::Buy);
                assert_eq!(t.symbol.base, "BTC");
                assert_eq!(t.symbol.quote, "USDT");
            }
            _ => panic!("expected Trade"),
        }
    }

    #[test]
    fn normalize_snapshot_creates_book_update() {
        let norm = FeedNormalizer::new(Venue::Binance, 2, 8);
        let msg = RawFeedMessage::OrderBookSnapshot {
            symbol: "ETHUSDT".to_string(),
            bids: vec![(3000.0, 10.0), (2999.0, 5.0)],
            asks: vec![(3001.0, 8.0)],
            timestamp: 2000,
        };
        let payload = norm.normalize(msg).unwrap();
        match payload {
            EventPayload::OrderBookUpdate(obu) => {
                assert!(obu.is_snapshot);
                assert_eq!(obu.bids.len(), 2);
                assert_eq!(obu.asks.len(), 1);
                assert_eq!(obu.symbol.base, "ETH");
            }
            _ => panic!("expected OrderBookUpdate"),
        }
    }

    #[test]
    fn normalize_delta_is_not_snapshot() {
        let norm = FeedNormalizer::new(Venue::Binance, 2, 8);
        let msg = RawFeedMessage::OrderBookDelta {
            symbol: "BTCUSDT".to_string(),
            bids: vec![(50000.0, 1.0)],
            asks: vec![],
            timestamp: 3000,
        };
        let payload = norm.normalize(msg).unwrap();
        match payload {
            EventPayload::OrderBookUpdate(obu) => {
                assert!(!obu.is_snapshot);
            }
            _ => panic!("expected OrderBookUpdate"),
        }
    }

    #[test]
    fn parse_symbol_formats() {
        let norm = FeedNormalizer::new(Venue::Binance, 2, 8);
        let s1 = norm.parse_symbol("BTCUSDT");
        assert_eq!(s1.base, "BTC");
        assert_eq!(s1.quote, "USDT");

        let s2 = norm.parse_symbol("BTC/USDT");
        assert_eq!(s2.base, "BTC");
        assert_eq!(s2.quote, "USDT");

        let s3 = norm.parse_symbol("ETH-BTC");
        assert_eq!(s3.base, "ETH");
        assert_eq!(s3.quote, "BTC");

        let s4 = norm.parse_symbol("SOLUSD");
        assert_eq!(s4.base, "SOL");
        assert_eq!(s4.quote, "USD");
    }

    #[test]
    fn sell_side_from_is_buy_false() {
        let norm = FeedNormalizer::new(Venue::Binance, 2, 8);
        let msg = RawFeedMessage::Trade {
            symbol: "BTCUSDT".to_string(),
            price: 50000.0,
            qty: 1.0,
            is_buy: false,
            timestamp: 0,
        };
        match norm.normalize(msg).unwrap() {
            EventPayload::Trade(t) => assert_eq!(t.side, Side::Sell),
            _ => panic!("expected Trade"),
        }
    }
}
