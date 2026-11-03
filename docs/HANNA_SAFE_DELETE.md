# HANNA Safe Delete

Delete marks the selected photos as Pending Delete. It does not change Picked,
Rejected, ratings, edit sidecars, or photo bytes. Marks and their last 64 undo/redo
actions are stored atomically in `.hanna-workflow.json` in the working folder.
Unknown versions or corrupt metadata fail closed and are never reset silently.

The Develop filmstrip has All Photos, Picked, Rejected, exact 1–5 star and
Pending Delete filters. Filtering keeps the catalog and existing thumbnails.
Use Left/Right to navigate, Ctrl-click to toggle selection, Shift-click or
Shift+Left/Right for a range, and Ctrl+A to select the filtered filmstrip.
The HANNA menu offers Pending Delete Undo/Redo and confirmed move/restore;
the photo context menu can restore marks without moving any files.

Only the explicitly confirmed **Move Marked Photos to HANNA_Deleted** action moves
files. A selected RAW includes in-camera JPG/JPEG/HEIC/HEIF files in the same
directory with the same case-insensitive stem. Selecting a rendered photo alone
does not select its RAW. Each included image brings its `.calibraw`,
`.calibraw-denoise`, `.calibraw-people`, developed-thumbnail companions, and source
XMP files named either `stem.xmp` or `filename.ext.xmp` (extension case does not
matter). Unrelated PNG/TIFF exports are not implicitly moved. XMP content is never
rewritten.

Every move gets a unique transaction directory inside `HANNA_Deleted`. Original
paths relative to the working folder map to `files/<original relative path>` in a
versioned `manifest.json`. The manifest is flushed before any move, before each
transition, and after completion. Windows moves use the existing atomic
no-replacement MoveFileExW implementation. Other systems require hard links and
refuse an unsupported filesystem rather than risk an overwriting rename.

**Restore to Original Folder** replays manifests, including interrupted moves and
restores. It restores exact original names. If the original name is occupied,
both files are retained and the conflict is reported; after resolving the
conflict the user can retry Restore. Missing files, sharing violations, permission
errors and partial operations are reported. No file is overwritten, automatically
renamed, permanently deleted, or purged by time. Completed journals remain in
HANNA_Deleted. A per-folder OS file lock serializes actions and is automatically
released if a process exits.

The Library scanner excludes HANNA_Deleted. System Recycle Bin remains a separate,
explicitly confirmed action, preserving the operating system's recovery workflow.
Tests only create and operate on temporary generated folders; real photo libraries
are never used as fixtures.
