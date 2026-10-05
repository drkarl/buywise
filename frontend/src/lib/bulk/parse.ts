// Bulk-add parsing: turn a deliberately pasted, multi-line grocery message into
// a list of proposed entries the user can edit before anything is queued.
//
// This module is PURE and CLIENT-SIDE: it never reads the clipboard, never calls
// the network, and never touches Dexie. The caller (BulkAddModal) owns the
// textarea, so pasted text is only ever processed after the user pastes it.
//
// It reuses the two existing pieces of the normal add-item flow rather than
// reimplementing them:
//   * `parseQuantityAndUnitSpans` (matching/normalize.ts) — the authoritative
//     count/size parser, so "2x eggs" and "500g mince" are read exactly as the
//     single-item input reads them.
//   * `checkForDuplicate` (matching/duplicate.ts) — the existing bilingual
//     duplicate cascade (exact → dictionary → fuzzy → contains), so in-paste and
//     against-the-active-list collisions follow the same rules as a normal add.
//
// Scope note (issue #115 open question): entries are ONE PER LINE. Comma- or
// prose-separated lists are deliberately NOT split — "milk, bread, eggs" stays a
// single line the user can edit or remove. Splitting prose reliably needs a
// different input contract and is a follow-up, not part of this flow.

import {
  parseQuantityAndUnitSpans,
  normalize,
  toDisplayName
} from '$lib/matching/normalize';
import { checkForDuplicate, type ActiveItem, type DuplicateResult } from '$lib/matching/duplicate';

/**
 * A leading list marker to strip from a pasted line: a bullet, a task checkbox,
 * or an ordinal ("1." / "2)"). The ordinal requires trailing whitespace, so a
 * decimal size like "2.5kg potatoes" keeps its number.
 */
const LIST_MARKER = /^\s*(?:[-*•·–—]|\[[ xX]\]|\d{1,3}[.)])\s+/;

/** Monotonic source of row ids; only uniqueness matters (rows are UI-local). */
let rowSeq = 0;

/** A proposed grocery entry parsed from one non-blank pasted line. */
export interface ParsedGroceryLine {
  /** UI-local row id (stable across re-renders while the row exists). */
  id: string;
  /** Cleaned display text: marker removed, pack count lifted into `quantity`. */
  text: string;
  /** Pack count from the line's count token (`"2x"`, `"dos"`), else 1. */
  quantity: number;
}

/** Where a proposed entry collides: the live list, or an earlier pasted line. */
export type DuplicateSource = 'list' | 'batch';

/** A proposed entry plus its duplicate classification (if any). */
export interface ProposedEntry extends ParsedGroceryLine {
  duplicate: DuplicateResult;
  duplicateSource: DuplicateSource | null;
}

/**
 * Splits pasted text into proposed entries, one per non-blank line.
 *
 * Every non-blank line yields exactly one entry — a line the parser cannot
 * confidently interpret is surfaced for the user to edit or remove, never
 * silently dropped. A recognised COUNT token ("2x", "dos") is lifted into the
 * editable `quantity`; a SIZE token ("500g", "dos litros") stays in the text
 * because the mutation protocol has no size field and dropping it would lose the
 * household's intent.
 */
export function parseGroceryText(pasted: string): ParsedGroceryLine[] {
  const entries: ParsedGroceryLine[] = [];

  for (const rawLine of pasted.split(/\r?\n/)) {
    const cleaned = rawLine.replace(LIST_MARKER, '').trim();
    if (!cleaned) continue; // blank / marker-only line: no input to preserve

    const spans = parseQuantityAndUnitSpans(cleaned);

    let display = cleaned;
    if (spans.countSpan) {
      const [start, end] = spans.countSpan;
      const withoutCount = `${cleaned.slice(0, start)} ${cleaned.slice(end)}`
        .replace(/\s+/g, ' ')
        .trim();
      // The shared parser only strips a count when residual text follows, so
      // this is non-empty in practice; guard anyway against a line whose residual
      // normalises away, and fall back to the raw line rather than emit nothing.
      if (withoutCount) display = withoutCount;
    }
    if (normalize(display) === '') display = cleaned;

    entries.push({
      id: `bulk-${++rowSeq}`,
      text: toDisplayName(display),
      quantity: spans.count
    });
  }

  return entries;
}

/**
 * Classifies each proposed entry against the active list and against the earlier
 * entries in the same paste (first occurrence wins: the later duplicate is the
 * one flagged). Uses the existing duplicate cascade unchanged, so a collision
 * here means exactly what the single-item input's warning would mean.
 */
export function evaluateProposals(
  entries: ParsedGroceryLine[],
  activeItems: ActiveItem[]
): ProposedEntry[] {
  const earlier: ActiveItem[] = [];
  const proposed: ProposedEntry[] = [];

  for (const entry of entries) {
    const againstList = checkForDuplicate(entry.text, activeItems).duplicate;
    let duplicate: DuplicateResult = againstList;
    let duplicateSource: DuplicateSource | null = againstList.found ? 'list' : null;

    if (!duplicate.found && earlier.length > 0) {
      const againstBatch = checkForDuplicate(entry.text, earlier).duplicate;
      if (againstBatch.found) {
        duplicate = againstBatch;
        duplicateSource = 'batch';
      }
    }

    proposed.push({ ...entry, duplicate, duplicateSource });
    earlier.push({ id: entry.id, display_name: entry.text });
  }

  return proposed;
}
