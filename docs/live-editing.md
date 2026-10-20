# Live editing through Craft Cloud

With the `live` feature (on by default in the desktop app; never in the web build), a `.deckcraft` file inside the
Craft Cloud folder opens live when the Craft Cloud agent runs: the same presentation open on several computers shows
every edit on the others as it is made. Anything else opens as before. `DECKCRAFT_NO_LIVE=1` turns it off.

- **What is shared:** slides and their order; shapes, groups and their properties (each property on its own, so two
  people changing different properties of one shape both keep theirs); text, tables and notes, character by
  character; comments, sections and custom shows; masters, layouts, the theme and the presentation's settings (each
  one whole: two changes to one master keep one); pictures, media and embedded fonts (their bytes through Craft
  Cloud's blob store). Selection, zoom, the current slide, panes and caches stay each computer's own.
- **Undo** takes back only this computer's changes. **Revert** replaces the live content with the file's, for
  everyone, as one change that Undo takes back.
- **Saving:** a live presentation autosaves a few seconds after a change, and when another computer's edits make the
  file behind. Two computers saving never make a conflict copy.
- **Media:** a picture or video goes to the other computers through Craft Cloud once its bytes are uploaded; until
  then the badge there says "waiting for media from <the computer that added it>", and the presentation saves once
  every item is here. Closing the window while media this computer added hasn't been sent waits for it, 15 s at
  most, and says so ("Close now" doesn't wait).
- **The window:** the status bar's badge says Live, Waiting, Offline or Read-only; Backstage shows the Craft Cloud
  account and the presentations open on every computer; a banner offers to bring in a file changed outside Craft;
  other computers' selections are outlined in their colour.
- **Big changes** (a paste of megabytes, deleting many slides) go whole: no computer ever shows part of one. One
  that came only part of the way (its sender stopped) is dropped, and the window catches up with Craft Cloud (the
  badge says how often). Every computer needs this DeckCraft and a Craft Cloud agent that knows such changes (older
  ones refuse them, and open this layout read-only).
- **Not live:** a presentation with a picture or video over 64 MB opens as a plain file, with a message.
- **Known gaps:**
  - Undo of a delete brings back a copy of what was deleted: edits another computer made to it in the meantime
    (typing, properties) are lost on both, and connector ends, comment anchors and animations that pointed at it stay
    cleared. Both computers still show the same thing.
  - Duplicating a slide keeps its comments anchored to the original slide's shapes (as without live). While live,
    the other computer clears those anchors.
  - Besides masters, layouts, the theme and settings, two more things are kept whole: a text box's body settings
    (autofit, insets and list style together) and a comment with its replies. Two replies added to one comment at
    once keep one.
  - A custom show lists each slide once: a slide listed twice keeps its first place.
- **The code:** `crates/engine/src/live/` (the adapter onto the Craft Cloud kit, and the session's hook, undo, open and
  save) and `crates/ui-egui/src/live.rs` (the window's piece). DeckCraft's ids stay local to each computer: the live
  document keys everything by its own element ids, and each session maps them.
