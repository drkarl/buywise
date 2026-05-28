# BuyWise

Private household grocery optimiser for Ireland. Compares prices across Tesco, Dunnes Stores, SuperValu, Aldi, and Lidl — recommends the cheapest single-store or multi-store plan for the week's shopping list.

Built for two adults and two children. Not a public product.

## What it does

- Collaborative shopping list with real-time sync across devices
- Bilingual entry (English and Spanish resolve to the same item)
- Weekly price data via BasketWatch (RapidAPI) and Lidl direct
- Four optimisation plans: best single store, best two-store split, theoretical minimum, and a recommended practical plan
- Offline-first PWA: the shopping list works without connectivity in-store
- Savings ledger: tracks what was actually saved over time

## Stack

| Layer | Choice |
|---|---|
| Backend | Rust + Axum |
| Frontend | SvelteKit (PWA, adapter-static) |
| Database | SQLite WAL + sqlx |
| Backup | Litestream → Cloudflare R2 |
| Hosting | Hetzner CAX11 ARM (~€5/month) |
| Real-time | Server-Sent Events |
| Offline storage | Dexie (IndexedDB) |

## Repository layout

```
backend/     Rust — Axum API, optimiser, ingestion adapters, CLI validation tool
frontend/    SvelteKit — PWA, shopping list UI, optimiser UI
docs/
  design/    Architecture and design documents
```

## Design document

See [`docs/design/design.md`](docs/design/design.md) for the full architecture, schema, sync protocol, bilingual resolution cascade, and phased build plan.

## Build phases

| Phase | Scope |
|---|---|
| 0 | CLI optimiser with fixture data — validates the core logic end-to-end |
| 1 | Axum API: auth, list CRUD, mutation endpoint, SSE stream |
| 2 | SvelteKit PWA: list UI, offline sync, shopping mode |
| 3 | Bilingual resolution cascade + LLM fallback |
| 4 | Optimiser UI: four plans, plain-language recommendation |
| 5 | Household learning, match feedback, savings ledger |
