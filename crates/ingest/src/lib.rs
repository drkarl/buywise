//! BuyWise ingest adapters.
//!
//! Fetches retailer price data from external sources and normalises it into the
//! `retailer_products` + `product_offers` tables. The BasketWatch RapidAPI
//! adapter ([`basketwatch`]) covers Tesco, Dunnes Stores, SuperValu and Aldi;
//! the Lidl adapter ([`lidl`], P0-T9) covers Lidl.ie directly and shares
//! [`product_id`]. The Lidl run is isolated from BasketWatch via
//! [`lidl::spawn_ingest`], so a Lidl failure cannot fail the BasketWatch run.

pub mod basketwatch;
pub mod error;
pub mod lidl;
pub mod product_id;
