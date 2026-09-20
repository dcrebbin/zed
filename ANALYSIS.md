# Crash Analysis: Edit prediction cursor popover row underflow

## Crash Summary

- **Sentry Issue:** Not provided
- **Error:** `attempt to subtract with overflow`
- **Crash Site:** `Editor::edit_prediction_cursor_popover_prefers_preview` in `crates/editor/src/edit_prediction.rs`

## Root Cause

Insertion predictions represent an empty edit range with a right-biased start anchor and a
left-biased end anchor. If the user inserts a newline at that position before the provider's
prediction refreshes, the tracked start anchor moves after the new line while the end anchor stays
before it. The cursor popover then resolves the start to a later row than the end and subtracts the
rows as unsigned integers while counting deleted newlines, causing an underflow.

## Reproduction

The regression test shows an insertion prediction, types a newline at its start, and asks the
expanded cursor popover to select its keybinding display. This reaches the same function and
panics at the same subtraction as the supplied crash traces.

Run it with:

`cargo test -p editor test_cursor_popover_keybind_after_typing_at_prediction_start`

## Suggested Fix

Use saturating subtraction when counting deleted newlines. A range whose tracked anchors have
temporarily crossed deletes zero lines; treating it as the absolute difference would incorrectly
classify the user's intervening insertion as a deletion.
