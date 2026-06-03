// Global test setup.
//
// `fake-indexeddb/auto` installs an in-memory IndexedDB implementation on the
// global scope so Dexie works under jsdom (which has no IndexedDB). Importing it
// here means every test file gets it without an explicit import.
import 'fake-indexeddb/auto';

// Adds jest-dom matchers (toBeInTheDocument, toBeDisabled, …) to Vitest's expect.
import '@testing-library/jest-dom/vitest';
