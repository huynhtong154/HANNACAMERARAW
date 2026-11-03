//! Pending marks and explicit recoverable folder operations, independent of photo reviews.
use super::{
    actions::library_actions_enabled, storage::desktop_paths, LibraryAsset, LibraryAssetId,
    LibraryState,
};
use crate::app::CalibRawApp;
use crate::i18n::{tr, tr_fmt};
use crate::services::safe_delete::{self, OperationReport, PendingDeleteStore};
use crate::sidecar::{PhotoFlag, PhotoReview};
use eframe::egui::{self, Align2, Color32, FontId, Ui};
use egui_phosphor::regular;
use std::path::PathBuf;
use std::sync::{atomic::Ordering, mpsc};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum FilmstripFilter {
    #[default]
    All,
    Picked,
    Rejected,
    Rating(u8),
    PendingDelete,
}

impl FilmstripFilter {
    pub(super) fn matches(self, review: PhotoReview, pending: bool) -> bool {
        match self {
            Self::All => true,
            Self::Picked => review.flag == PhotoFlag::Picked,
            Self::Rejected => review.flag == PhotoFlag::Rejected,
            Self::Rating(rating) => review.rating == rating,
            Self::PendingDelete => pending,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum WorkflowOperation {
    Move,
    Restore,
}

type Completion = (PathBuf, Result<OperationReport, String>);

#[derive(Default)]
pub(super) struct HannaWorkflow {
    loaded_folder: Option<PathBuf>,
    store: Option<PendingDeleteStore>,
    quarantined: usize,
    pub(super) filter: FilmstripFilter,
    undo_context: bool,
    pub(super) confirmation: Option<WorkflowOperation>,
    pub(super) result_message: Option<String>,
    pub(super) receiver: Option<mpsc::Receiver<Completion>>,
}

impl LibraryState {
    pub(crate) fn hanna_file_operation_in_progress(&self) -> bool {
        self.workflow.receiver.is_some()
    }
    pub(crate) fn workflow_status(&self) -> &str {
        &self.status
    }
    pub(crate) fn is_pending_delete(&self, asset: &LibraryAsset) -> bool {
        asset.desktop_path().is_some_and(|path| {
            self.workflow
                .store
                .as_ref()
                .is_some_and(|store| store.is_pending(path))
        })
    }

    pub(crate) fn leave_pending_undo_context(&mut self) {
        self.workflow.undo_context = false;
    }

    pub(crate) fn mark_pending_delete(&mut self, assets: &[LibraryAsset], pending: bool) {
        if self.local_mutation_in_progress() {
            self.status = tr("Another library file action is still running.").to_owned();
            return;
        }
        let Some(store) = self.workflow.store.as_mut() else {
            self.status =
                tr("Pending Delete is unavailable until the folder metadata can be read.")
                    .to_owned();
            return;
        };
        let paths = desktop_paths(assets);
        match store.set_pending(&paths, pending) {
            Ok(count) => {
                if count > 0 {
                    self.workflow.undo_context = true;
                }
                self.status = tr_fmt(
                    if pending {
                        "Marked {count} photos Pending Delete; originals are unchanged."
                    } else {
                        "Restored {count} Pending Delete marks; originals are unchanged."
                    },
                    &[("count", &count)],
                );
            }
            Err(error) => self.status = format!("Could not save Pending Delete: {error:#}"),
        }
    }

    pub(super) fn poll_hanna_workflow(&mut self, context: &egui::Context) {
        let result = self
            .workflow
            .receiver
            .as_ref()
            .map(mpsc::Receiver::try_recv);
        match result {
            Some(Ok((folder, result))) => {
                self.workflow.receiver = None;
                if self.folder.as_ref() == Some(&folder) {
                    self.workflow.loaded_folder = None;
                    self.refresh(context);
                }
                self.status = match result {
                    Ok(report) => format!("HANNA file operation: {} files completed. {}", report.files, report.failures.join(" · ")),
                    Err(error) => format!("HANNA file operation stopped: {error}. Use Restore to Original Folder to recover journaled files."),
                };
                self.workflow.result_message = Some(self.status.clone());
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.workflow.receiver = None;
                self.status =
                    tr("The HANNA file worker stopped. The recovery manifest has been retained.")
                        .to_owned();
                self.workflow.result_message = Some(self.status.clone());
            }
            Some(Err(mpsc::TryRecvError::Empty)) => {
                context.request_repaint_after(Duration::from_millis(50))
            }
            None => {}
        }
        if self.workflow.loaded_folder != self.folder {
            self.workflow.loaded_folder = self.folder.clone();
            self.workflow.store = None;
            self.workflow.quarantined = 0;
            self.workflow.undo_context = false;
            if let Some(folder) = &self.folder {
                match PendingDeleteStore::load(folder) {
                    Ok(store) => {
                        self.workflow.quarantined =
                            match safe_delete::quarantined_files(store.root()) {
                                Ok(count) => count,
                                Err(error) => {
                                    self.status = format!(
                                        "Could not inspect HANNA recovery manifests: {error:#}"
                                    );
                                    0
                                }
                            };
                        self.workflow.store = Some(store);
                    }
                    Err(error) => {
                        self.status = format!("Could not load HANNA workflow metadata: {error:#}")
                    }
                }
            }
        }
    }

    pub(crate) fn filmstrip_selected(&self, asset: &LibraryAssetId) -> bool {
        self.is_selected(asset)
    }

    pub(crate) fn select_filmstrip_item(&mut self, asset: &LibraryAssetId, range: bool) {
        if range {
            let visible = self.filmstrip_entry_indices();
            self.select_range_to(asset, &visible);
        } else {
            self.toggle_selected(asset);
        }
    }

    pub(crate) fn anchor_filmstrip_selection(&mut self, asset: &LibraryAssetId) {
        if !self.is_selecting() {
            self.activate_id(asset.clone());
        }
    }

    pub(crate) fn select_filmstrip_all(&mut self) {
        let visible = self.filmstrip_entry_indices();
        self.clear_selection();
        for index in visible {
            let id = self.entries[index].asset.id.clone();
            self.add_to_selection(&id);
        }
    }
}

pub(crate) fn handle_workflow_shortcuts(app: &mut CalibRawApp, context: &egui::Context) -> bool {
    if !app.navigation_shortcuts_allowed(context)
        || !app.library.workflow.undo_context
        || app.library.local_mutation_in_progress()
    {
        return false;
    }
    let Some(store) = app.library.workflow.store.as_mut() else {
        return false;
    };
    let redo = egui::KeyboardShortcut::new(
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        egui::Key::Z,
    );
    let undo = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Z);
    let redo_y = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Y);
    let action = context.input_mut(|input| {
        if store.can_redo() && (input.consume_shortcut(&redo) || input.consume_shortcut(&redo_y)) {
            Some(true)
        } else if store.can_undo() && input.consume_shortcut(&undo) {
            Some(false)
        } else {
            None
        }
    });
    let Some(redo) = action else {
        return false;
    };
    let result = if redo { store.redo() } else { store.undo() };
    app.library.status = match result {
        Ok(_) => tr(if redo {
            "Redid Pending Delete marks"
        } else {
            "Undid Pending Delete marks"
        })
        .to_owned(),
        Err(error) => format!("Could not update Pending Delete: {error:#}"),
    };
    true
}

pub(crate) fn show_filmstrip_filter(ui: &mut Ui, app: &mut CalibRawApp) {
    egui::ScrollArea::horizontal()
        .id_salt("hanna-filmstrip-filters")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let filter = &mut app.library.workflow.filter;
                ui.selectable_value(filter, FilmstripFilter::All, tr("All Photos"));
                ui.selectable_value(filter, FilmstripFilter::Picked, tr("Picked"));
                ui.selectable_value(filter, FilmstripFilter::Rejected, tr("Rejected"));
                for rating in 1..=5 {
                    ui.selectable_value(
                        filter,
                        FilmstripFilter::Rating(rating),
                        format!("{rating}★"),
                    );
                }
                ui.selectable_value(filter, FilmstripFilter::PendingDelete, tr("Pending Delete"));
                ui.separator();
                show_workflow_button(ui, app);
            });
        });
}

pub(super) fn show_workflow_button(ui: &mut Ui, app: &mut CalibRawApp) {
    let response = moduwu_design::toolbar_button(ui, format!("{} HANNA", regular::TRASH), 85.0)
        .on_hover_text(&app.library.status);
    moduwu_design::dropdown_menu(&response, |ui| show_workflow_menu(ui, app));
}

pub(super) fn show_workflow_menu(ui: &mut Ui, app: &mut CalibRawApp) {
    let enabled = !app.library.local_mutation_in_progress() && app.library.workflow.store.is_some();
    let pending = app
        .library
        .workflow
        .store
        .as_ref()
        .map_or(0, |store| store.pending_paths().len());
    if ui
        .add_enabled(
            enabled && pending > 0,
            moduwu_design::MenuItem::new(tr("Move Marked Photos to HANNA_Deleted…"))
                .icon(regular::TRASH),
        )
        .clicked()
    {
        app.library.workflow.confirmation = Some(WorkflowOperation::Move);
        ui.close();
    }
    if ui
        .add_enabled(
            enabled && app.library.workflow.quarantined > 0,
            moduwu_design::MenuItem::new(tr("Restore to Original Folder…"))
                .icon(regular::ARROW_COUNTER_CLOCKWISE),
        )
        .clicked()
    {
        app.library.workflow.confirmation = Some(WorkflowOperation::Restore);
        ui.close();
    }
    let (can_undo, can_redo) = app
        .library
        .workflow
        .store
        .as_ref()
        .map_or((false, false), |store| (store.can_undo(), store.can_redo()));
    for (redo, label, available) in [
        (false, "Undo Pending Delete", can_undo),
        (true, "Redo Pending Delete", can_redo),
    ] {
        if ui
            .add_enabled(
                enabled && available,
                moduwu_design::MenuItem::new(tr(label)),
            )
            .clicked()
        {
            if let Some(store) = app.library.workflow.store.as_mut() {
                let result = if redo { store.redo() } else { store.undo() };
                if let Err(error) = result {
                    app.library.status = format!("Could not update Pending Delete: {error:#}");
                }
                app.library.workflow.undo_context = true;
            }
            ui.close();
        }
    }
}

pub(super) fn show_workflow_confirmation(ui: &mut Ui, app: &mut CalibRawApp) {
    if let Some(message) = &app.library.workflow.result_message {
        let mut close = false;
        moduwu_design::Dialog::new("hanna-file-operation-result", tr("HANNA file operation"))
            .show_with_footer(
                ui.ctx(),
                |ui| {
                    ui.label(message);
                },
                |ui| {
                    close = moduwu_design::secondary_button(ui, tr("Close")).clicked()
                        || moduwu_design::dialog_keyboard_action(
                            ui,
                            moduwu_design::DialogKeyboard::CLOSE_ONLY,
                            false,
                        ) == moduwu_design::DialogAction::Cancel;
                },
            );
        if close {
            app.library.workflow.result_message = None;
        }
        return;
    }
    let Some(operation) = app.library.workflow.confirmation else {
        return;
    };
    let moving = matches!(operation, WorkflowOperation::Move);
    let title = if moving {
        "Move Marked Photos to HANNA_Deleted?"
    } else {
        "Restore to Original Folder?"
    };
    let mut action = moduwu_design::DialogAction::None;
    moduwu_design::Dialog::new("hanna-file-operation-confirmation", tr(title)).show_with_footer(ui.ctx(), |ui| {
        let count = if moving { app.library.workflow.store.as_ref().map_or(0, |store| store.pending_paths().len()) } else { app.library.workflow.quarantined };
        ui.label(tr_fmt(if moving { "{count} marked photos" } else { "{count} quarantined files" }, &[("count", &count)]));
        ui.label(if moving {
            tr("Moves marked originals into HANNA_Deleted inside the active folder. A marked RAW brings matching JPEG/HEIC companions; a marked rendered photo moves only itself. Native sidecars and matching XMP files follow. A recovery manifest records every original path. Nothing is permanently deleted.")
        } else {
            tr("Restores journaled files to their exact original paths. Existing files are never overwritten. Conflicts and interrupted operations remain in HANNA_Deleted for recovery.")
        });
        if let Some(folder) = app.library.folder() { ui.monospace(folder.display().to_string()); }
    }, |ui| {
        action = moduwu_design::dialog_confirmation_actions(ui, tr("Cancel"), tr(if moving { "Move marked photos" } else { "Restore files" }), false, true, moduwu_design::DialogKeyboard::CLOSE_ONLY);
    });
    match action {
        moduwu_design::DialogAction::Cancel => app.library.workflow.confirmation = None,
        moduwu_design::DialogAction::Confirm => {
            app.library.workflow.confirmation = None;
            start_operation(app, operation, ui.ctx());
        }
        moduwu_design::DialogAction::None => {}
    }
}

fn start_operation(app: &mut CalibRawApp, operation: WorkflowOperation, context: &egui::Context) {
    if !library_actions_enabled(app) {
        return;
    }
    if app.develop.load_receiver.is_some() || app.foreground_operation_active() {
        app.library.status =
            tr("Wait for the current photo operation to finish before moving files.").to_owned();
        return;
    }
    let Some(folder) = app.library.folder.clone() else {
        return;
    };
    app.settle_library_review_saves();
    if matches!(operation, WorkflowOperation::Move) {
        let marked = app
            .library
            .workflow
            .store
            .as_ref()
            .map_or_else(Vec::new, PendingDeleteStore::pending_paths);
        let affects_current = app.develop.current_path.as_ref().is_some_and(|current| {
            marked.iter().any(|path| {
                path == current
                    || (path.parent() == current.parent()
                        && path.file_stem() == current.file_stem())
            })
        });
        if affects_current {
            if let Err(error) = app.close_document_for_hanna_move() {
                app.library.status = error;
                return;
            }
        }
    }
    // Cancel queued reads without discarding resident thumbnails. The worker
    // waits for the decode gate, so an in-flight thumbnail/cache write finishes
    // before the first filesystem move; waiting never blocks the UI thread.
    app.library.generation.fetch_add(1, Ordering::AcqRel);
    app.library.event_receiver = None;
    app.library.request_sender = None;
    app.library.scanning = false;
    let decode_gate = app.library.decode_gate();
    let (sender, receiver) = mpsc::channel();
    let worker_folder = folder;
    let repaint = context.clone();
    match std::thread::Builder::new()
        .name("hanna-recoverable-files".to_owned())
        .spawn(move || {
            let _decode_guard = decode_gate
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let result = match operation {
                WorkflowOperation::Move => safe_delete::move_marked(&worker_folder),
                WorkflowOperation::Restore => safe_delete::restore_all(&worker_folder),
            }
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send((worker_folder, result));
            repaint.request_repaint();
        }) {
        Ok(_) => {
            app.library.workflow.receiver = Some(receiver);
            app.library.status = tr("HANNA file operation in progress…").to_owned();
            context.request_repaint();
        }
        Err(error) => {
            app.library.refresh(context);
            app.library.status = format!("Could not start HANNA file worker: {error}");
        }
    }
}

pub(crate) fn paint_pending_delete(ui: &Ui, rect: egui::Rect) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        rect,
        moduwu_design::CARD_RADIUS,
        Color32::from_black_alpha(115),
    );
    painter.text(
        rect.right_top() + egui::vec2(-15.0, 15.0),
        Align2::CENTER_CENTER,
        regular::TRASH,
        FontId::proportional(20.0),
        Color32::WHITE,
    );
}

#[cfg(test)]
mod tests {
    use super::super::{
        new_library_entry, scan_folder_tree, scan_folder_with_limit, LibraryPreferences,
        LibrarySortOrder, LibraryThumbnailSize,
    };
    use super::*;
    use crate::platform::PlatformCaps;
    use anyhow::Context;
    use std::fs;

    #[test]
    fn filmstrip_filters_and_pending_marks_reuse_catalog_without_decoding_or_replacing_textures(
    ) -> anyhow::Result<()> {
        let folder = tempfile::tempdir()?;
        let first = folder.path().join("first.NEF");
        let second = folder.path().join("second.NEF");
        fs::write(&first, b"original first")?;
        fs::write(&second, b"original second")?;
        let mut library = LibraryState::new_desktop(
            crate::platform::fake::FakePlatform::new(PlatformCaps::DESKTOP),
            LibraryPreferences {
                thumbnail_workers: 1,
                automatic_thumbnail_workers: false,
                thumbnail_size: LibraryThumbnailSize::Large,
                sort_order: LibrarySortOrder::NameAscending,
                stack_raw_companions: false,
                render_edited_thumbnails_during_indexing: false,
            },
            true,
        );
        library.folder = Some(folder.path().to_path_buf());
        let mut first_asset = LibraryAsset::from_desktop_path(first.clone(), 14, 1, None);
        first_asset.metadata.review = PhotoReview {
            flag: PhotoFlag::Picked,
            rating: 5,
        };
        let mut second_asset = LibraryAsset::from_desktop_path(second.clone(), 15, 1, None);
        second_asset.metadata.review = PhotoReview {
            flag: PhotoFlag::Rejected,
            rating: 1,
        };
        library.entries = vec![
            new_library_entry(first_asset.clone()),
            new_library_entry(second_asset),
        ];
        library.sort_entries();
        let context = egui::Context::default();
        let texture = context.load_texture(
            "retained",
            egui::ColorImage::new([1, 1], vec![Color32::WHITE]),
            egui::TextureOptions::LINEAR,
        );
        library.entries[0].texture = Some(texture.clone());
        library.poll_hanna_workflow(&context);
        let generation = library.generation.load(Ordering::Acquire);
        library.workflow.filter = FilmstripFilter::Picked;
        assert_eq!(library.filmstrip_entry_indices(), [0]);
        library.workflow.filter = FilmstripFilter::Rejected;
        assert_eq!(library.filmstrip_entry_indices(), [1]);
        library.workflow.filter = FilmstripFilter::Rating(5);
        assert_eq!(library.filmstrip_entry_indices(), [0]);
        library.mark_pending_delete(std::slice::from_ref(&first_asset), true);
        library.workflow.filter = FilmstripFilter::PendingDelete;
        assert_eq!(library.filmstrip_entry_indices(), [0]);
        assert_eq!(library.entries[0].review().flag, PhotoFlag::Picked);
        assert_eq!(
            library.entries[0]
                .texture
                .as_ref()
                .map(egui::TextureHandle::id),
            Some(texture.id())
        );
        assert_eq!(library.generation.load(Ordering::Acquire), generation);
        assert!(library.request_sender.is_none());
        assert!(!library.scanning);
        assert!(library
            .workflow
            .store
            .as_mut()
            .context("missing mark store")?
            .undo()?);
        assert!(library.filmstrip_entry_indices().is_empty());
        assert_eq!(fs::read(first)?, b"original first");
        assert_eq!(fs::read(second)?, b"original second");
        Ok(())
    }

    #[test]
    fn scanner_excludes_quarantine_even_when_opened_directly() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        fs::write(root.path().join("visible.NEF"), b"raw")?;
        let quarantine = root.path().join("hanna_deleted");
        fs::create_dir(&quarantine)?;
        fs::write(quarantine.join("hidden.NEF"), b"raw")?;
        let tree = scan_folder_tree(root.path(), || false).context("missing folder tree")?;
        assert!(tree.children.is_empty());
        assert!(scan_folder_with_limit(&quarantine, 10, || false).is_err());
        let scan = scan_folder_with_limit(root.path(), 10, || false)
            .map_err(anyhow::Error::msg)?
            .context("missing catalog")?;
        assert_eq!(scan.assets.len(), 1);
        assert_eq!(scan.assets[0].display_name, "visible.NEF");
        Ok(())
    }
}
