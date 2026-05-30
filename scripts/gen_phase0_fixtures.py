#!/usr/bin/env python3
"""One-off generator for the P0-T11 basket/offer fixtures.

Produces, consistently against fixtures/canonical_items.json:
  * fixtures/offers.json          (BasketWatch-format snapshot)
  * fixtures/product_matches.csv  (canonical_item_id -> retailer_product_id)
  * fixtures/weekly-shop.json     (~40-line weekly shop, mixed EN/ES)
  * fixtures/items.json           (~30-line validation basket, mixed EN/ES)

Resolution/pricing consistency is guaranteed by construction: every weekly-shop
line is a canonical item's own name_en / name_es (which resolve via exact alias
to that item's id), and offers + product_matches are generated for those same
canonical ids, so every priced line is genuinely priceable.

retailer_product_id is the stable SHA-256 of (source:retailer:external_id),
matching crates/ingest/src/product_id.rs.

This script is committed for reproducibility; the JSON/CSV it emits are the real
deliverables. It is not run by the build. Run it from anywhere:
    python3 scripts/gen_phase0_fixtures.py
"""
import csv
import hashlib
import io
import json
import os

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FIX = os.path.join(ROOT, "fixtures")

# The only valid BasketWatch retailers (must match the schema CHECK + Retailer
# enum). A price row keyed by anything else is a data bug and is dropped with a
# warning rather than silently emitted as an invalid retailer.
VALID_RETAILERS = {"tesco", "dunnes", "supervalu", "aldi"}


def stable_product_id(source, retailer, external_id):
    key = f"{source}:{retailer}:{external_id.strip()}"
    return hashlib.sha256(key.encode()).hexdigest()


with open(os.path.join(FIX, "canonical_items.json"), encoding="utf-8") as f:
    CANON_DOC = json.load(f)
ITEMS = CANON_DOC["canonical_items"]
BY_SLUG = {i["slug"]: i for i in ITEMS}
BY_NAME_EN = {i["name_en"].lower(): i for i in ITEMS}


def find_item(*candidates):
    """Find a canonical item by trying slug then lowercase name_en variants."""
    for c in candidates:
        if c in BY_SLUG:
            return BY_SLUG[c]
        if c.lower() in BY_NAME_EN:
            return BY_NAME_EN[c.lower()]
        for alt in (c.replace("_", " "), c.replace(" ", "_")):
            if alt in BY_SLUG:
                return BY_SLUG[alt]
    return None


# Canonical slugs emitted in BOTH languages in the weekly shop, to exercise
# bilingual resolution. A generous set so the basket lands at ~40 lines.
BILINGUAL_PAIRS = {
    "whole milk", "bread", "eggs", "cheddar cheese", "rice",
    "banana", "apple", "coffee", "tomato", "chicken breast",
}

# One spec per item:
#   (candidate names, (pack_size_text, size_value, size_unit, pack_count),
#    name_tail, {retailer: (price_cents, loyalty_cents, scheme, brand)})
# Prices are arranged so no single store wins everything (Aldi cheapest on most
# staples; Tesco/SuperValu/Dunnes win several via loyalty), giving an achievable
# saving well over EUR50/month.
SPEC = [
    (["whole milk", "whole_milk", "milk"], ("2L", 2000.0, "ml", 1), "Whole Milk 2L", {
        "aldi": (229, None, None, "Cowbelle"),
        "tesco": (245, 179, "clubcard", "Tesco"),
        "supervalu": (239, None, None, "SuperValu"),
    }),
    (["eggs"], ("6 pack", None, None, 6), "Free Range Eggs 6 Pack", {
        "dunnes": (139, None, None, "Dunnes Stores"),
        "aldi": (199, None, None, "Merevale"),
        "tesco": (209, None, None, "Tesco"),
    }),
    (["bread", "white bread"], ("800g", 800.0, "g", 1), "White Sliced Pan 800g", {
        "aldi": (115, None, None, "Village Bakery"),
        "tesco": (149, 79, "clubcard", "Tesco"),
        "supervalu": (155, None, None, "SuperValu"),
    }),
    (["butter"], ("227g", 227.0, "g", 1), "Pure Irish Butter 227g", {
        "aldi": (259, None, None, "Greenvale"),
        "tesco": (279, None, None, "Kerrygold"),
        "supervalu": (289, 199, "real_rewards", "Kerrygold"),
    }),
    (["cheddar cheese", "cheddar_cheese", "cheese"], ("400g", 400.0, "g", 1), "Mature Cheddar 400g", {
        "aldi": (349, None, None, "The Cheese Emporium"),
        "tesco": (379, 279, "clubcard", "Tesco"),
        "supervalu": (389, None, None, "SuperValu"),
    }),
    (["rice", "white rice"], ("1kg", 1000.0, "g", 1), "Basmati Rice 1kg", {
        "aldi": (159, None, None, "Worldwide Foods"),
        "tesco": (219, None, None, "Tesco"),
    }),
    (["spaghetti", "pasta"], ("500g", 500.0, "g", 1), "Spaghetti 500g", {
        "aldi": (65, None, None, "Cucina"),
        "tesco": (89, None, None, "Tesco"),
        "supervalu": (159, None, None, "Barilla"),
    }),
    (["banana", "bananas"], ("1kg", 1000.0, "g", 1), "Bananas per kg", {
        "aldi": (149, None, None, None),
        "tesco": (169, None, None, None),
    }),
    (["apple", "apples"], ("1kg", 1000.0, "g", 1), "Apples 1kg", {
        "supervalu": (199, None, None, "SuperValu"),
        "aldi": (215, None, None, None),
        "tesco": (229, None, None, "Tesco"),
    }),
    (["coffee"], ("200g", 200.0, "g", 1), "Coffee 200g", {
        "aldi": (499, None, None, "Alcafe"),
        "supervalu": (729, None, None, "Nescafe"),
        "tesco": (749, 549, "clubcard", "Nescafe"),
    }),
    (["tea", "green tea", "black tea"], ("80 bags", None, None, 80), "Tea 80 Bags", {
        "dunnes": (379, None, None, "Barrys"),
        "supervalu": (389, None, None, "Lyons"),
        "tesco": (429, 399, "clubcard", "Barrys"),
    }),
    (["sugar"], ("1kg", 1000.0, "g", 1), "Granulated Sugar 1kg", {
        "aldi": (99, None, None, "Aldi"),
        "tesco": (139, None, None, "Tesco"),
    }),
    (["flour"], ("1kg", 1000.0, "g", 1), "Plain Flour 1kg", {
        "aldi": (79, None, None, "Aldi"),
        "supervalu": (145, None, None, "Odlums"),
    }),
    (["chicken breast", "chicken_breast", "chicken"], ("1kg", 1000.0, "g", 1), "Chicken Fillets 1kg", {
        "dunnes": (599, None, None, "Dunnes Stores"),
        "aldi": (799, None, None, "Ashfield"),
        "tesco": (829, 749, "clubcard", "Tesco"),
    }),
    (["tomato", "tomatoes"], ("6 pack", None, None, 6), "Tomatoes 6 Pack", {
        "supervalu": (149, None, None, "SuperValu"),
        "tesco": (179, None, None, "Tesco"),
    }),
    (["onion", "onions"], ("1kg", 1000.0, "g", 1), "Brown Onions 1kg", {
        "aldi": (79, None, None, None),
        "supervalu": (119, None, None, "SuperValu"),
    }),
    (["potato", "potatoes"], ("2.5kg", 2500.0, "g", 1), "Rooster Potatoes 2.5kg", {
        "aldi": (199, None, None, None),
        "dunnes": (249, None, None, "Dunnes Stores"),
    }),
    (["carrot", "carrots"], ("1kg", 1000.0, "g", 1), "Carrots 1kg", {
        "aldi": (49, None, None, None),
        "tesco": (89, None, None, "Tesco"),
    }),
    (["orange juice", "orange_juice"], ("1L", 1000.0, "ml", 1), "Orange Juice 1L", {
        "supervalu": (289, None, None, "Tropicana"),
        "tesco": (299, 269, "clubcard", "Tropicana"),
    }),
    (["cereal", "corn flakes", "cornflakes"], ("500g", 500.0, "g", 1), "Corn Flakes 500g", {
        "aldi": (109, None, None, "Harvest Morn"),
        "tesco": (329, None, None, "Kelloggs"),
    }),
    (["tuna"], ("4 pack", None, None, 4), "Tuna Chunks 4 x 145g", {
        "dunnes": (449, None, None, "John West"),
        "supervalu": (499, None, None, "John West"),
    }),
    (["olive oil", "olive_oil", "oil"], ("500ml", 500.0, "ml", 1), "Olive Oil 500ml", {
        "aldi": (329, None, None, "Solesta"),
        "tesco": (449, None, None, "Tesco"),
    }),
    (["chocolate"], ("180g", 180.0, "g", 1), "Dairy Milk Chocolate 180g", {
        "dunnes": (200, None, None, "Cadbury"),
        "tesco": (250, 215, "clubcard", "Cadbury"),
    }),
    (["toilet paper", "toilet_paper", "toilet roll"], ("9 rolls", None, None, 9), "Toilet Tissue 9 Rolls", {
        "aldi": (299, None, None, "Saxon"),
        "tesco": (399, None, None, "Tesco"),
    }),
    (["laundry detergent", "laundry_detergent", "detergent"], ("30 wash", None, None, 30), "Washing Liquid 30 Wash", {
        "supervalu": (699, None, None, "Ariel"),
        "tesco": (799, 749, "clubcard", "Ariel"),
    }),
    (["yogurt", "yoghurt"], ("500g", 500.0, "g", 1), "Natural Yogurt 500g", {
        "supervalu": (219, None, None, "Glenisk"),
        "tesco": (249, None, None, "Glenisk"),
    }),
    (["nappies_size_4"], ("44 pack", None, None, 44), "Nappies Size 4 (44 pack)", {
        "aldi": (399, None, None, "Mamia"),
        "tesco": (699, 549, "clubcard", "Pampers"),
    }),
    (["nappies_size_5"], ("40 pack", None, None, 40), "Nappies Size 5 (40 pack)", {
        "aldi": (459, None, None, "Mamia"),
        "dunnes": (399, None, None, "Pampers"),
    }),
    # Dish soap, sold only as Fairy 500ml. fixtures/item_constraints.json carries
    # a Fairy brand + 500ml constraint for this item; the dictionary policy is
    # `any` so live and fixture modes behave identically (live mode does not seed
    # item_constraints). No own-label option, so the basket stays complete.
    (["dish soap", "dish_soap"], ("500ml", 500.0, "ml", 1), "Washing Up Liquid 500ml", {
        "tesco": (250, 200, "clubcard", "Fairy"),
        "supervalu": (269, None, None, "Fairy"),
    }),
]


# The "stocks everything" baseline store. Every item is guaranteed a Tesco
# offer so the best single-store plan can always be COMPLETE (missing_count==0),
# while cheaper Aldi/Dunnes/SuperValu offers keep the two-store split and the
# theoretical minimum strictly below the single-store total. Tesco is never made
# the cheapest option (its synthesised price sits above the existing minimum).
BASELINE_RETAILER = "tesco"
# Brand to attach to a synthesised Tesco baseline offer when the item is
# otherwise own-label (None brand) — Tesco own-label.
BASELINE_BRAND = "Tesco"


def resolve_spec():
    resolved = []
    for cand, size, name_tail, prices in SPEC:
        item = find_item(*cand)
        if item is None:
            raise SystemExit(f"no canonical item for {cand} (regenerate canonical_items.json?)")
        # Drop any invalid retailer key defensively.
        clean_prices = {}
        for retailer, val in prices.items():
            if retailer not in VALID_RETAILERS:
                print(f"  WARNING: dropping invalid retailer {retailer!r} for {cand}")
                continue
            clean_prices[retailer] = val

        # Guarantee a baseline (Tesco) offer so best_single is always complete.
        # Skip the brand-constrained dish soap (Fairy-only by design).
        if BASELINE_RETAILER not in clean_prices and "dish" not in cand[0]:
            cheapest = min(p[0] for p in clean_prices.values())
            # Price the baseline above the cheapest so Tesco never wins on price.
            baseline_price = cheapest + 60
            # A synthesised baseline product is Tesco's own product, so it must
            # carry Tesco's OWN brand — never a competitor's. Reusing a sibling
            # retailer's brand here produced impossible committed rows (a Tesco
            # product named/branded "Aldi Plain Flour", "SuperValu Brown Onions",
            # etc.). Tesco own-label is always brand-valid.
            clean_prices[BASELINE_RETAILER] = (baseline_price, None, None, BASELINE_BRAND)

        resolved.append((item, size, name_tail, clean_prices))
    return resolved


def build():
    resolved = resolve_spec()
    offers = []
    matches = []
    weekly = []
    seen_weekly_ids = set()

    for (item, size, name_tail, prices) in resolved:
        psize, sval, sunit, pc = size
        for retailer, (price, loy, scheme, brand) in prices.items():
            ext = f"{retailer}-{item['slug'].replace(' ', '')}"
            rpid = stable_product_id("basketwatch", retailer, ext)
            full_name = f"{brand + ' ' if brand else ''}{name_tail}".strip()
            offers.append({
                "external_id": ext,
                "retailer": retailer,
                "name": full_name,
                "brand": brand,
                "category": item["category_key"].title(),
                "url": f"https://example.test/{retailer}/{ext}",
                "pack_size_text": psize,
                "size_value": sval,
                "size_unit": sunit,
                "pack_count": pc,
                "price_cents": price,
                "unit_price_eur_micros": None,
                "unit_price_basis": ("per_l" if sunit == "ml" else "per_kg" if sunit == "g" else "per_each"),
                "previous_price_cents": None,
                "loyalty_price_cents": loy,
                "loyalty_scheme": scheme,
                "offer_text": (f"{scheme} price" if scheme else None),
                "offer_valid_until": "2026-06-30",
                "source_snapshot_date": "2026-05-29",
                "cache_expires_at": "2026-06-05T03:00:00.000Z",
            })
            matches.append((item["id"], rpid, "accepted", "system"))

        if item["id"] not in seen_weekly_ids:
            seen_weekly_ids.add(item["id"])
            # Nappies stay as canonical items + offers + matches (satisfying the
            # "separate slugs nappies_size_4 / nappies_size_5" requirement) but
            # are NOT put on a basket line: "Nappies Size 4" and "Nappies Size 5"
            # are within fuzzy distance of each other and resolve ambiguously, so
            # a basket line for them would never price. They are exercised by the
            # data layer, not the basket.
            if item["slug"] in ("nappies_size_4", "nappies_size_5"):
                continue
            has_es = item["name_es"] and item["name_es"].lower() != item["name_en"].lower()
            use_es = (len(seen_weekly_ids) % 2 == 0) and has_es
            weekly.append(item["name_es"] if use_es else item["name_en"])
            if item["slug"] in BILINGUAL_PAIRS and has_es:
                weekly.append(item["name_en"] if use_es else item["name_es"])

    # Orphan product with a null external_id (skipped on ingest). Required by the
    # issue note about offers.json.
    offers.append({
        "external_id": None,
        "retailer": "tesco",
        "name": "Mystery Product With No External ID",
        "brand": None,
        "category": None,
        "url": None,
        "pack_size_text": None,
        "size_value": None,
        "size_unit": None,
        "pack_count": None,
        "price_cents": 999,
        "unit_price_eur_micros": None,
        "unit_price_basis": None,
        "previous_price_cents": None,
        "loyalty_price_cents": None,
        "loyalty_scheme": None,
        "offer_text": None,
        "offer_valid_until": None,
        "source_snapshot_date": "2026-05-29",
        "cache_expires_at": None,
    })

    return offers, matches, weekly


def write_offers(offers):
    with open(os.path.join(FIX, "offers.json"), "w", encoding="utf-8") as f:
        json.dump(offers, f, ensure_ascii=False, indent=2)
        f.write("\n")


def write_matches(matches):
    buf = io.StringIO()
    buf.write("# product_matches.csv - canonical_item -> retailer_product mappings (P0-T11).\n")
    buf.write("# retailer_product_id = SHA-256(basketwatch:<retailer>:<external_id>), per\n")
    buf.write("# crates/ingest/src/product_id.rs. Regenerate: scripts/gen_phase0_fixtures.py\n")
    w = csv.writer(buf, lineterminator="\n")
    w.writerow(["canonical_item_id", "retailer_product_id", "status", "matched_by"])
    for r in matches:
        w.writerow(r)
    with open(os.path.join(FIX, "product_matches.csv"), "w", encoding="utf-8") as f:
        f.write(buf.getvalue())


def write_weekly(weekly):
    doc = {
        "_note": ("Weekly shop for Phase-0 optimise (P0-T11). Lines are the canonical "
                  "name_en/name_es of items that have offers + matches, so each resolves "
                  "(bilingually) to a priceable canonical item. Phase-0 constraint scope "
                  "is size + brand only; nappies are separate slugs (nappies_size_4 / "
                  "nappies_size_5). Regenerate: scripts/gen_phase0_fixtures.py"),
        "household_id": "00000000-0000-0000-0000-000000000001",
        "items": weekly,
    }
    with open(os.path.join(FIX, "weekly-shop.json"), "w", encoding="utf-8") as f:
        json.dump(doc, f, ensure_ascii=False, indent=2)
        f.write("\n")


def write_items(weekly):
    base = weekly[:24]
    extra = ["Jamon", "Milk", "leche", "queso", "pan", "huevos"]
    items = base + extra
    doc = {
        "_note": ("Validation basket for Phase-0 validate-basket (P0-T11). Mixed EN/ES "
                  "staples that resolve via alias or fuzzy match, giving a resolution rate "
                  ">= 90%. Regenerate: scripts/gen_phase0_fixtures.py"),
        "household_id": "00000000-0000-0000-0000-000000000001",
        "items": items,
    }
    with open(os.path.join(FIX, "items.json"), "w", encoding="utf-8") as f:
        json.dump(doc, f, ensure_ascii=False, indent=2)
        f.write("\n")


if __name__ == "__main__":
    offers, matches, weekly = build()
    write_offers(offers)
    write_matches(matches)
    write_weekly(weekly)
    write_items(weekly)
    print(f"wrote {len(offers)} offers, {len(matches)} matches, {len(weekly)} weekly lines")
