//! BuyWise ingest adapters.
//!
//! Fetches retailer price data from external sources and normalises it into the
//! `retailer_products` + `product_offers` tables. The BasketWatch RapidAPI
//! adapter ([`basketwatch`]) covers Tesco, Dunnes Stores, SuperValu and Aldi;
//! the Lidl adapter (P0-T9) is added separately and shares [`product_id`].

pub mod basketwatch;
pub mod error;
pub mod product_id;
