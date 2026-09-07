use std::{
    collections::{HashMap, HashSet},
    ops::Deref,
};

use chrono::NaiveDateTime;
use thiserror::Error;

use crate::{
    db::{
        Attachment, AttachmentId, CustomIcon, CustomIconId, Entry, EntryId, Group, GroupId, GroupRef, History,
        Icon, MoveGroupError, Times,
    },
    Database,
};

#[derive(Debug, Clone)]
pub enum MergeEventType {
    Created,
    Deleted,
    LocationUpdated,
    Updated,
}

#[derive(Debug, Clone)]
pub enum MergeEventTarget {
    Entry(EntryId),
    Group(GroupId),
}

#[derive(Debug, Clone)]
pub struct MergeEvent {
    pub target: MergeEventTarget,
    pub event_type: MergeEventType,
}

/// Errors while merge two databases
#[derive(Error)]
#[derive(Debug)]
pub enum MergeError {
    #[error("Entries with UUID {0} have the same modification time but have diverged.")]
    EntryModificationTimeNotUpdated(EntryId),

    #[error("Groups with UUID {0} have the same modification time but have diverged.")]
    GroupModificationTimeNotUpdated(GroupId),

    #[error("Found history entries with the same timestamp ({0}) for entry {1}.")]
    DuplicateHistoryEntries(NaiveDateTime, EntryId),

    #[error("Attachment {0} is referenced by an entry but missing from the source database.")]
    AttachmentNotFound(AttachmentId),

    #[error(transparent)]
    MoveGroupError(#[from] MoveGroupError),
}

#[derive(Debug, Default, Clone)]
pub struct MergeLog {
    pub warnings: Vec<String>,
    pub events: Vec<MergeEvent>,
}

impl Database {
    /// Merge a database with another version of the same database, applying the changes to self.
    ///
    /// This function will use the UUIDs to detect what entries and groups are the same.
    pub fn merge(&mut self, other: &Database) -> Result<MergeLog, MergeError> {
        let mut log = MergeLog::default();
        merge_groups(self, other, &mut log)?;
        // The merge copies icon *references* from source onto destination
        // objects. Copy the images those references point at before the
        // reference index is rebuilt, or the destination ends up with
        // CustomIconUUIDs no client can resolve.
        adopt_source_custom_icons(self, other);
        rebuild_attachment_references(self);
        rebuild_custom_icon_references(self);

        Ok(log)
    }
}

/// Clone an entry from another database and translate every database-local
/// attachment ID (including historical versions) into the destination's ID
/// space. Equal attachment values are shared; new values are copied once.
fn import_entry(dest_db: &mut Database, source_db: &Database, source: &Entry) -> Result<Entry, MergeError> {
    let mut imported = source.clone();
    remap_entry_attachments(dest_db, source_db, &mut imported)?;
    Ok(imported)
}

fn remap_entry_attachments(
    dest_db: &mut Database,
    source_db: &Database,
    entry: &mut Entry,
) -> Result<(), MergeError> {
    remap_attachment_map(dest_db, source_db, &mut entry.attachments)?;
    if let Some(history) = entry.history.as_mut() {
        for historical in &mut history.entries {
            remap_attachment_map(dest_db, source_db, &mut historical.attachments)?;
        }
    }
    Ok(())
}

fn remap_attachment_map(
    dest_db: &mut Database,
    source_db: &Database,
    attachments: &mut HashMap<String, AttachmentId>,
) -> Result<(), MergeError> {
    for source_id in attachments.values_mut() {
        let source_attachment = source_db
            .attachments
            .get(source_id)
            .ok_or(MergeError::AttachmentNotFound(*source_id))?;

        let dest_id = dest_db
            .attachments
            .iter()
            .find_map(|(id, attachment)| (attachment.data == source_attachment.data).then_some(*id))
            .unwrap_or_else(|| {
                let id = AttachmentId::next_free(dest_db);
                dest_db.attachments.insert(
                    id,
                    Attachment {
                        id,
                        entries: HashSet::new(),
                        data: source_attachment.data.clone(),
                    },
                );
                id
            });
        *source_id = dest_id;
    }
    Ok(())
}

/// Entry attachment maps are authoritative. Rebuild the inverse reference
/// index after merge and drop blobs that no current or historical entry uses.
fn rebuild_attachment_references(db: &mut Database) {
    let mut references: HashMap<AttachmentId, HashSet<(EntryId, Option<usize>)>> = HashMap::new();
    for (&entry_id, entry) in &db.entries {
        for &attachment_id in entry.attachments.values() {
            references
                .entry(attachment_id)
                .or_default()
                .insert((entry_id, None));
        }
        if let Some(history) = &entry.history {
            for (index, historical) in history.entries.iter().enumerate() {
                for &attachment_id in historical.attachments.values() {
                    references
                        .entry(attachment_id)
                        .or_default()
                        .insert((entry_id, Some(index)));
                }
            }
        }
    }

    db.attachments
        .retain(|id, attachment| match references.remove(id) {
            Some(entries) => {
                attachment.entries = entries;
                true
            }
            None => false,
        });
}

/// Copy every custom icon the merged destination now references but does not
/// own. Icon references travel with entries and groups during a merge, while
/// the images live in a database-local table, so without this step a merged
/// vault renders default icons and loses the images on the next save.
///
/// The source UUID is kept whenever it is free, so two databases of the same
/// lineage converge on one icon table instead of minting a fresh id on every
/// merge. Equal images are shared, and an id already taken by a *different*
/// image gets a new one.
fn adopt_source_custom_icons(dest_db: &mut Database, source_db: &Database) {
    let mut translations: HashMap<CustomIconId, Option<CustomIconId>> = HashMap::new();
    for id in referenced_custom_icons(dest_db) {
        if dest_db.custom_icons.contains_key(&id) {
            continue;
        }
        let translated = source_db.custom_icons.get(&id).map(|source_icon| {
            let shared = dest_db
                .custom_icons
                .values()
                .find(|existing| existing.data == source_icon.data)
                .map(|existing| existing.id);
            shared.unwrap_or_else(|| {
                // Keep the source uuid: it is free here by construction (the
                // caller only asks about ids the destination lacks), and
                // reusing it is what lets two copies of the same vault
                // converge on one icon table instead of minting a fresh id on
                // every merge.
                let new_id = id;
                dest_db.custom_icons.insert(
                    new_id,
                    CustomIcon {
                        id: new_id,
                        entries: HashSet::new(),
                        groups: HashSet::new(),
                        data: source_icon.data.clone(),
                        name: source_icon.name.clone(),
                        last_modification_time: source_icon.last_modification_time,
                    },
                );
                new_id
            })
        });
        translations.insert(id, translated);
    }
    if translations.is_empty() {
        return;
    }

    // `None` means the source referenced an image it does not carry either.
    // Dropping the reference is the only lossless outcome: a dangling
    // CustomIconUUID is not readable by any client.
    let retarget = |icon: &mut Option<Icon>| {
        if let Some(Icon::Custom(id)) = icon {
            if let Some(translated) = translations.get(id) {
                *icon = translated.map(Icon::Custom);
            }
        }
    };
    for entry in dest_db.entries.values_mut() {
        retarget(&mut entry.icon);
        if let Some(history) = entry.history.as_mut() {
            for historical in &mut history.entries {
                retarget(&mut historical.icon);
            }
        }
    }
    for group in dest_db.groups.values_mut() {
        retarget(&mut group.icon);
    }
}

/// Every custom icon id an entry, an entry's history, or a group points at.
fn referenced_custom_icons(db: &Database) -> HashSet<CustomIconId> {
    let mut referenced = HashSet::new();
    let mut note = |icon: &Option<Icon>| {
        if let Some(Icon::Custom(id)) = icon {
            referenced.insert(*id);
        }
    };
    for entry in db.entries.values() {
        note(&entry.icon);
        if let Some(history) = &entry.history {
            for historical in &history.entries {
                note(&historical.icon);
            }
        }
    }
    for group in db.groups.values() {
        note(&group.icon);
    }
    referenced
}

/// Entry and group icon fields are authoritative. Rebuild the inverse
/// reference index after a merge and drop images nothing points at, mirroring
/// what `rebuild_attachment_references` does for attachment blobs.
pub(crate) fn rebuild_custom_icon_references(db: &mut Database) {
    let mut entries: HashMap<CustomIconId, HashSet<(EntryId, Option<usize>)>> = HashMap::new();
    let mut groups: HashMap<CustomIconId, HashSet<GroupId>> = HashMap::new();

    for (&entry_id, entry) in &db.entries {
        if let Some(Icon::Custom(id)) = entry.icon {
            entries.entry(id).or_default().insert((entry_id, None));
        }
        if let Some(history) = &entry.history {
            for (index, historical) in history.entries.iter().enumerate() {
                if let Some(Icon::Custom(id)) = historical.icon {
                    entries.entry(id).or_default().insert((entry_id, Some(index)));
                }
            }
        }
    }
    for (&group_id, group) in &db.groups {
        if let Some(Icon::Custom(id)) = group.icon {
            groups.entry(id).or_default().insert(group_id);
        }
    }

    db.custom_icons.retain(|id, icon| {
        let entry_refs = entries.remove(id);
        let group_refs = groups.remove(id);
        if entry_refs.is_none() && group_refs.is_none() {
            return false;
        }
        icon.entries = entry_refs.unwrap_or_default();
        icon.groups = group_refs.unwrap_or_default();
        true
    });
}

/// Get the last update time (modification or location change) of a group, considering its entries and subgroups.
fn get_last_update(group: GroupRef<'_>) -> Option<NaiveDateTime> {
    let last_update = group.times.last_modification.or(group.times.location_changed);

    group
        .entries()
        .filter_map(|e| e.times.last_modification.or(e.times.location_changed))
        .chain(
            group
                .groups()
                .filter_map(|g| g.times.last_modification.or(g.times.location_changed)),
        )
        .chain(last_update)
        .max()
}

/// Merge groups from `source` into `dest`, appending to a log of the merge process.
///
/// NOTE: this function will also call `merge_entries` to handle entries within the groups.
fn merge_groups(dest_db: &mut Database, source_db: &Database, log: &mut MergeLog) -> Result<(), MergeError> {
    let dest_groups = dest_db.groups.keys().cloned().collect::<HashSet<_>>();
    let source_groups = source_db.groups.keys().cloned().collect::<HashSet<_>>();

    // Handle groups that exist only in source and might need to be added.
    let mut groups_to_add = HashSet::new();
    for &id in source_groups.difference(&dest_groups) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let source = source_db.group(id).unwrap();

        // was the group deleted in dest?
        if let Some(deletion_time) = dest_db.deleted_objects.get(&id.uuid()) {
            // get the last modification time of the group in source.
            let source_last_update = get_last_update(source);

            // compare deletion time and last update time to decide whether to re-add the group
            match (deletion_time, source_last_update) {
                (Some(deletion_time), Some(source_last_update)) => {
                    // if the group was deleted after its last modification time in source,
                    // do not re-add it, otherwise we can re-add the group
                    if *deletion_time >= source_last_update {
                        continue;
                    }
                }
                (Some(_), None) => {
                    // blank last update time in source - do not re-add the group
                    continue;
                }
                (None, Some(_)) => {
                    // blank deletion time is probably older than concrete update time - re-add the
                    // group
                }
                (None, None) => {
                    // both times are blank - do not re-add the group
                    continue;
                }
            }
        }

        groups_to_add.insert(id);
    }

    // actually add groups from groups_to_add. Use a stack to ensure that parent groups are added as needed
    let mut add_stack = Vec::new();
    loop {
        // refill the stack if it's empty
        if add_stack.is_empty() {
            if let Some(&next) = groups_to_add.iter().next() {
                // refill the stack with an arbitrary group to re-add
                add_stack.push(next);
                groups_to_add.remove(&next);
            } else {
                // no more groups to re-add
                break;
            }
        }

        // get the current group from the stack
        #[allow(clippy::expect_used)] // stack is guaranteed to be non-empty
        let &id = add_stack.last().expect("non-empty queue");

        // get the desired parent of the group to be re-added
        #[allow(clippy::expect_used)] // id is guaranteed to exist in source
        let source = source_db.group(id).expect("source group exists");

        #[allow(clippy::expect_used)] // this would be a severe issue with the algorithm
        let parent_id = source.parent().expect("cannot re-add root").id();

        // does the parent exist in dest?
        if let Some(mut parent) = dest_db.group_mut(parent_id) {
            // yes - re-add the group
            let mut dest_group = parent.add_group_with_id(id);
            dest_group.times = source.times.clone();
            dest_group.name = source.name.clone();
            dest_group.notes = source.notes.clone();
            dest_group.icon = source.icon.clone();
            dest_group.custom_data = source.custom_data.clone();
            dest_group.is_expanded = source.is_expanded;
            dest_group.default_autotype_sequence = source.default_autotype_sequence.clone();
            dest_group.enable_autotype = source.enable_autotype;
            dest_group.enable_searching = source.enable_searching;
            dest_group.last_top_visible_entry = source.last_top_visible_entry;
            dest_group.previous_parent_group = source.previous_parent_group;
            dest_group.tags = source.tags.clone();

            log.events.push(MergeEvent {
                target: MergeEventTarget::Group(id),
                event_type: MergeEventType::Created,
            });

            // success - remove the current item from the stack (it was already removed from the set)
            add_stack.pop();
        } else {
            // the parent does not exist yet - add it to the stack to be re-added first
            add_stack.push(parent_id);

            // since we will deal with the parent now, it doesn't need to be handled later
            groups_to_add.remove(&parent_id);
        }
    }

    // Handle groups that exist only in destination. These groups might need to be deleted.
    let mut to_delete = Vec::new();
    for &id in dest_groups.difference(&source_groups) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let dest = dest_db.group_mut(id).unwrap();

        // was the group deleted in source?
        if let Some(deletion_time) = source_db.deleted_objects.get(&id.uuid()) {
            let dest_last_updated = get_last_update(dest.as_ref());
            if let (Some(deletion_time), Some(dest_last_updated)) = (deletion_time, dest_last_updated) {
                // if the group was deleted and then later modified in dest, do not delete it
                if *deletion_time < dest_last_updated {
                    continue;
                }
            }

            // queue the deletion so that all subgroups will also emit a deletion event
            to_delete.push(id);
            dest_db.deleted_objects.insert(id.uuid(), *deletion_time);

            log.events.push(MergeEvent {
                target: MergeEventTarget::Group(id),
                event_type: MergeEventType::Deleted,
            });
        }
    }

    // perform the entry merges now that all groups that need adding are added but the groups that
    // need deleting still haven't been deleted, so that the entries can still be accessed and
    // generate events
    merge_entries(dest_db, source_db, log)?;

    // perform all group deletions
    while let Some(id) = to_delete.pop() {
        if let Some(group) = dest_db.group_mut(id) {
            group.remove();
        }
    }

    // re-compute the group set after additions and deletions
    let dest_groups = dest_db.groups.keys().cloned().collect::<HashSet<_>>();

    // Handle groups that exist in both source and destination.
    let mut moves = Vec::new();
    let root_id = dest_db.root().id();
    for &id in dest_groups.intersection(&source_groups) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let mut dest = dest_db.group_mut(id).unwrap();

        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let source = source_db.group(id).unwrap();

        let dest_parent_id = dest.as_ref().parent().map(|p| p.id());
        let source_parent_id = source.parent().map(|p| p.id());

        // was the group moved?
        if dest_parent_id != source_parent_id {
            let dest_location_changed = dest.times.location_changed;
            let source_location_changed = source.times.location_changed;

            if let (Some(dlc), Some(slc)) = (dest_location_changed, source_location_changed) {
                if slc > dlc {
                    // the source group has been moved more recently than the destination group.
                    // try to move the destination group to the new location.

                    let Some(parent_id) = source.parent().map(|p| p.id()) else {
                        log.warnings.push(format!("Cannot move root group {}", id,));
                        continue;
                    };

                    if !dest_groups.contains(&parent_id) {
                        log.warnings.push(format!(
                            "Cannot move group {} to group {} because the group does not exist in the destination database.",
                            id,
                            parent_id,
                        ));
                        continue;
                    };

                    // to avoid creating cycles in situations where two groups swap their parent-child
                    // relationship, move all groups to root first and then to their final destination
                    moves.push((id, parent_id));
                    dest.move_to(root_id)?;
                    dest.times.location_changed = Some(slc);

                    log.events.push(MergeEvent {
                        target: MergeEventTarget::Group(id),
                        event_type: MergeEventType::LocationUpdated,
                    });
                }
            } else {
                log.warnings.push(format!(
                    "Cannot determine which group {} move is more recent because one of the groups does not have a location changed timestamp.",
                    id,
                ));
            }
        }

        let dest_last_modification = dest.times.last_modification.unwrap_or_else(|| {
            log.warnings.push(format!(
                "Destination group {} did not have a last modification timestamp",
                id
            ));
            Times::now()
        });

        let source_last_modification = source.times.last_modification.unwrap_or_else(|| {
            log.warnings.push(format!(
                "Source group {} did not have a last modification timestamp",
                id
            ));
            Times::epoch()
        });

        if dest_last_modification == source_last_modification {
            if have_groups_diverged(&dest, &source) {
                // This should never happen.
                //
                // A group was updated without updating the last modification timestamp.
                return Err(MergeError::GroupModificationTimeNotUpdated(id));
            }
            continue;
        }

        if dest_last_modification > source_last_modification {
            // The destination group is more recent than the source group. Nothing to do.
            continue;
        }

        // The source group is more recent than the destination group. Update dest with source.
        dest.name = source.name.clone();
        dest.notes = source.notes.clone();
        dest.icon = source.icon.clone();
        dest.custom_data = source.custom_data.clone();
        dest.times.last_modification = source.times.last_modification.or(dest.times.last_modification);
        dest.is_expanded = source.is_expanded;
        dest.default_autotype_sequence = source.default_autotype_sequence.clone();
        dest.enable_autotype = source.enable_autotype;
        dest.enable_searching = source.enable_searching;
        dest.last_top_visible_entry = source.last_top_visible_entry;

        log.events.push(MergeEvent {
            target: MergeEventTarget::Group(id),
            event_type: MergeEventType::Updated,
        });
    }

    // perform all the moves that were queued up
    for (group_id, parent_id) in moves {
        #[allow(clippy::unwrap_used)] // group_id and parent_id are guaranteed to exist
        let mut group = dest_db.group_mut(group_id).unwrap();
        group.move_to(parent_id)?;
    }

    Ok(())
}

/// Merge entries from `source` into `dest`, appending to a log of the merge process.
fn merge_entries(dest_db: &mut Database, source_db: &Database, log: &mut MergeLog) -> Result<(), MergeError> {
    let dest_entries = dest_db.entries.keys().cloned().collect::<HashSet<_>>();
    let source_entries = source_db.entries.keys().cloned().collect::<HashSet<_>>();

    // Handle entries that exist only in source and might need to be added.
    for &id in source_entries.difference(&dest_entries) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let source_entry = source_db.entry(id).unwrap();

        // was the entry deleted in dest?
        if let Some(deletion_time) = dest_db.deleted_objects.get(&id.uuid()) {
            // get the last modification or location change time in source.
            let source_update_time = source_entry
                .times
                .last_modification
                .or(source_entry.times.location_changed);

            match (deletion_time, source_update_time) {
                (Some(deletion_time), Some(source_update_time)) => {
                    // if the entry was deleted after its last modification time in source,
                    // do not re-add it
                    if *deletion_time >= source_update_time {
                        continue;
                    }
                }
                (Some(_), None) => {
                    // blank last update time in source - do not re-add the entry
                    continue;
                }
                (None, Some(_)) => {
                    // blank deletion time is probably older than concrete update time - re-add the
                    // entry
                }
                (None, None) => {
                    // both times are blank - do not re-add the entry
                    continue;
                }
            }

            // otherwise, we can re-add the entry
        }

        let parent_id = source_entry.parent().id();

        if dest_db.group(parent_id).is_none() {
            log.warnings.push(format!(
                "Cannot add entry {} because its parent group {} does not exist in the destination database.",
                id, parent_id,
            ));
            continue;
        }

        let imported = import_entry(dest_db, source_db, &source_entry)?;
        let mut parent = dest_db.group_mut(parent_id).expect("parent checked above");
        let mut entry = parent.add_entry_with_id(id);
        *entry = imported;

        log.events.push(MergeEvent {
            target: MergeEventTarget::Entry(id),
            event_type: MergeEventType::Created,
        });
    }

    // Handle entries that exist only in destination. These entries might need to be deleted.
    for &id in dest_entries.difference(&source_entries) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let dest_entry = dest_db.entry_mut(id).unwrap();

        // was the entry deleted in source?
        if let Some(deletion_time) = source_db.deleted_objects.get(&id.uuid()) {
            let dest_update_time = dest_entry
                .times
                .last_modification
                .or(dest_entry.times.location_changed);

            if let (Some(deletion_time), Some(dest_update_time)) = (deletion_time, dest_update_time) {
                // if the entry was deleted and then later modified in dest, do not delete it
                if *deletion_time < dest_update_time {
                    continue;
                }
            }

            dest_entry.remove();
            dest_db.deleted_objects.insert(id.uuid(), *deletion_time);

            log.events.push(MergeEvent {
                target: MergeEventTarget::Entry(id),
                event_type: MergeEventType::Deleted,
            });
        }
    }

    // Handle entries that exist in both source and destination.
    for &id in dest_entries.intersection(&source_entries) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist in source
        let source_entry = source_db.entry(id).unwrap();
        let source_entry = import_entry(dest_db, source_db, &source_entry)?;

        #[allow(clippy::unwrap_used)] // id is guaranteed to exist in both dest and source
        let mut dest_entry = dest_db.entry_mut(id).unwrap();

        let dest_parent_id = dest_entry.as_ref().parent().id();
        let source_parent_id = source_entry.parent;

        // has the entry moved?
        if dest_parent_id != source_parent_id {
            // which move is more recent?
            let source_location_changed = source_entry.times.location_changed;
            let dest_location_changed = dest_entry.times.location_changed;
            if let (Some(slc), Some(dlc)) = (source_location_changed, dest_location_changed) {
                if slc > dlc {
                    // the source entry has been moved more recently than the destination entry.
                    // try to move the destination entry to the new location.

                    if dest_entry.move_to(source_parent_id).is_ok() {
                        log.events.push(MergeEvent {
                            target: MergeEventTarget::Entry(id),
                            event_type: MergeEventType::LocationUpdated,
                        });
                        dest_entry.times.location_changed = Some(slc);
                    } else {
                        log.warnings.push(format!(
                            "Cannot move entry {} to group {} because the group does not exist in the destination database.",
                            id,
                            source_parent_id,
                        ));
                    }
                }
            } else {
                log.warnings.push(format!(
                    "Cannot determine which entry {} move is more recent because one of the entries does not have a location changed timestamp.",
                    id,
                ));
            }
        }

        let source_last_modification = source_entry.times.last_modification.unwrap_or_else(|| {
            log.warnings.push(format!(
                "Source entry {} did not have a last modification timestamp",
                id
            ));
            Times::epoch()
        });

        let dest_last_modification = dest_entry.times.last_modification.unwrap_or_else(|| {
            log.warnings.push(format!(
                "Destination entry {} did not have a last modification timestamp",
                id
            ));
            Times::now()
        });

        if dest_last_modification == source_last_modification {
            if have_entries_diverged(&dest_entry, &source_entry) {
                // This should never happen.
                //
                // An entry was updated without updating the last modification timestamp.
                return Err(MergeError::EntryModificationTimeNotUpdated(id));
            }
            continue;
        }

        let source_history = source_entry.history.clone().unwrap_or_else(|| {
            log.warnings.push(format!("Source entry {} had no history.", id));
            History::default()
        });

        let dest_history = dest_entry.history.clone().unwrap_or_else(|| {
            log.warnings
                .push(format!("Destination entry {} had no history.", id));
            History::default()
        });

        let mut merged_history = merge_history(&dest_history, &source_history, log)?;
        let merged_location_timestamp = dest_entry
            .times
            .location_changed
            .or(source_entry.times.location_changed);

        if source_last_modification > dest_last_modification {
            // add the previous dest entry to history if it has diverged
            let should_archive_dest = merged_history
                .entries
                .first()
                .is_none_or(|last_history_entry| have_entries_diverged(&dest_entry, last_history_entry));
            if should_archive_dest {
                let mut dest_entry_for_history = dest_entry.deref().clone();
                dest_entry_for_history.history = None;
                merged_history.add_entry(dest_entry_for_history);
            }

            // The source entry is more recent than the destination entry. Replace dest with source.
            dest_entry.times.last_modification = source_entry.times.last_modification;
            dest_entry.fields = source_entry.fields.clone();
            dest_entry.autotype = source_entry.autotype.clone();
            dest_entry.tags = source_entry.tags.clone();
            dest_entry.custom_data = source_entry.custom_data.clone();
            dest_entry.icon = source_entry.icon.clone();
            dest_entry.foreground_color = source_entry.foreground_color.clone();
            dest_entry.background_color = source_entry.background_color.clone();
            dest_entry.override_url = source_entry.override_url.clone();
            dest_entry.quality_check = source_entry.quality_check;
            dest_entry.previous_parent_group = source_entry.previous_parent_group;

            dest_entry.attachments = source_entry.attachments.clone();

            log.events.push(MergeEvent {
                target: MergeEventTarget::Entry(id),
                event_type: MergeEventType::Updated,
            });
        }

        dest_entry.history = Some(merged_history);
        dest_entry.times.location_changed = merged_location_timestamp;
    }

    Ok(())
}

/// Merge two histories together, returning the merged history.
fn merge_history(dest: &History, source: &History, log: &mut MergeLog) -> Result<History, MergeError> {
    let mut entries: Vec<Entry> = Vec::new();

    let mut entries_dest: Vec<Entry> = dest.entries.to_vec();
    let mut entries_source: Vec<Entry> = source.entries.to_vec();

    for e in entries_dest.iter_mut() {
        if e.times.last_modification.is_none() {
            log.warnings.push(format!(
                "Destination history entry {} did not have a last modification timestamp",
                e.id()
            ));
            e.times.last_modification = Some(Times::epoch());
        }
    }

    for e in entries_source.iter_mut() {
        if e.times.last_modification.is_none() {
            log.warnings.push(format!(
                "Source history entry {} did not have a last modification timestamp",
                e.id()
            ));
            e.times.last_modification = Some(Times::epoch());
        }
    }

    entries_dest.sort_by_key(|e| e.times.last_modification);
    entries_source.sort_by_key(|e| e.times.last_modification);

    // perform a merge of both histories, which are sorted by last modification time.
    //
    // this code has a lot of unwraps but they are all checked - entry lists are checked for
    // emptiness, and times are made not-none before sorting, so the unwraps should never panic.
    #[allow(clippy::unwrap_used)]
    loop {
        match (entries_dest.is_empty(), entries_source.is_empty()) {
            (false, false) => {
                // Both histories have entries left to process.
                let dest_entry = entries_dest.last().unwrap();
                let source_entry = entries_source.last().unwrap();

                let dest_time = dest_entry.times.last_modification.unwrap();

                let source_time = source_entry.times.last_modification.unwrap();

                if dest_time > source_time {
                    entries.push(entries_dest.pop().unwrap());
                } else if source_time > dest_time {
                    entries.push(entries_source.pop().unwrap());
                } else if have_entries_diverged(dest_entry, source_entry) {
                    log.warnings.push(format!(
                        "History entries for {} have the same modification timestamp {} but have diverged.",
                        dest_entry.id(),
                        source_time,
                    ));

                    // Both entries have the same timestamp but are different.
                    entries.push(entries_dest.pop().unwrap());
                    entries.push(entries_source.pop().unwrap());
                } else {
                    // The entries are the same, so we can just take one of them.
                    entries.push(entries_dest.pop().unwrap());
                    entries_source.pop();
                }
            }

            (true, false) => {
                // Only the source history has entries left to process - just take them all.
                entries.push(entries_source.pop().unwrap());
            }
            (false, true) => {
                // Only the destination history has entries left to process - just take them all.
                entries.push(entries_dest.pop().unwrap());
            }
            (true, true) => break,
        }
    }

    Ok(History { entries })
}

/// Check if two groups are dissimilar, ignoring their timestamps and pure
/// UI view state. `is_expanded` and `last_top_visible_entry` are written by
/// clients (e.g. KeePassXC) without bumping the modification time, so two
/// otherwise-identical files routinely differ on them with tied timestamps —
/// treating that as divergence would fail the merge for view-only changes.
fn have_groups_diverged(a: &Group, b: &Group) -> bool {
    let new_times = Times::default();

    let mut a = a.clone();
    a.times = new_times.clone();
    a.entries.clear();
    a.groups.clear();
    a.parent = None;
    a.is_expanded = false;
    a.last_top_visible_entry = None;

    let mut b = b.clone();
    b.times = new_times.clone();
    b.entries.clear();
    b.groups.clear();
    b.parent = None;
    b.is_expanded = false;
    b.last_top_visible_entry = None;

    !a.eq(&b)
}

/// Check if two entries are dissimilar, ignoring their timestamps.
fn have_entries_diverged(a: &Entry, b: &Entry) -> bool {
    let new_times = Times::default();

    let mut a = a.clone();
    a.times = new_times.clone();
    a.history = None;

    let mut b = b.clone();
    b.times = new_times.clone();
    b.history = None;

    !a.eq(&b)
}

#[cfg(test)]
mod merge_tests {
    use uuid::uuid;

    use crate::db::{fields, AttachmentId, EntryId, GroupId, History, Times, Value};
    use crate::Database;

    const ROOT_GROUP_ID: GroupId = GroupId::from_uuid(uuid!("00000000-0000-0000-0000-000000000001"));
    const GROUP1_ID: GroupId = GroupId::from_uuid(uuid!("00000000-0000-0000-0000-000000000002"));
    const GROUP2_ID: GroupId = GroupId::from_uuid(uuid!("00000000-0000-0000-0000-000000000003"));
    const SUBGROUP1_ID: GroupId = GroupId::from_uuid(uuid!("00000000-0000-0000-0000-000000000004"));
    const SUBGROUP2_ID: GroupId = GroupId::from_uuid(uuid!("00000000-0000-0000-0000-000000000005"));
    const ENTRY1_ID: EntryId = EntryId::from_uuid(uuid!("00000000-0000-0000-0000-000000000006"));
    const ENTRY2_ID: EntryId = EntryId::from_uuid(uuid!("00000000-0000-0000-0000-000000000007"));

    /// A merge copies icon references from source onto destination objects.
    /// The images live in a database-local table, so the destination has to
    /// take a copy or it renders a default icon and loses the image on save.
    #[test]
    fn merging_an_entry_carries_its_custom_icon_image() {
        let mut dest = create_test_database();
        let mut source = dest.clone();

        let image = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        let icon_id = source
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .edit(|e| e.set_unprotected("Title", "renamed remotely"))
            .set_icon_custom_new(image.clone())
            .id();
        source.entry_mut(ENTRY1_ID).unwrap().times.last_modification =
            Some(Times::now() + chrono::Duration::seconds(60));

        dest.merge(&source).expect("merge");

        let entry = dest.entry(ENTRY1_ID).expect("entry survives the merge");
        let icon = entry.custom_icon().expect("icon reference resolves");
        assert_eq!(icon.data, image, "the image travelled with the reference");
        assert_eq!(
            icon.id(),
            icon_id,
            "the source uuid is kept so both sides converge on one icon table"
        );
    }

    /// Two databases that already share an image must not accumulate a second
    /// copy of it on every merge.
    #[test]
    fn merging_shares_an_image_the_destination_already_has() {
        let mut dest = create_test_database();
        let image = vec![1, 2, 3, 4];
        dest.entry_mut(ENTRY1_ID)
            .unwrap()
            .set_icon_custom_new(image.clone());

        // The same image on the other side, under its own uuid: what two
        // machines produce when both fetch the same favicon.
        let mut source = dest.clone();
        source
            .entry_mut(ENTRY2_ID)
            .unwrap()
            .edit(|e| e.set_unprotected("Title", "renamed remotely"))
            .set_icon_custom_new(image.clone());
        source.entry_mut(ENTRY2_ID).unwrap().times.last_modification =
            Some(Times::now() + chrono::Duration::seconds(60));

        dest.merge(&source).expect("merge");

        assert_eq!(
            dest.num_custom_icons(),
            1,
            "the same image is stored once, not once per merge"
        );
    }

    /// Clearing an icon remotely must not delete the image locally while the
    /// version the merge archives still renders it.
    #[test]
    fn merging_keeps_an_image_the_archived_version_still_renders() {
        let mut dest = create_test_database();
        dest.entry_mut(ENTRY1_ID)
            .unwrap()
            .set_icon_custom_new(vec![9, 9, 9]);
        assert_eq!(dest.num_custom_icons(), 1);

        let mut source = dest.clone();
        source
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .edit(|e| e.set_icon_none())
            .times
            .last_modification = Some(Times::now() + chrono::Duration::seconds(60));

        dest.merge(&source).expect("merge");

        assert!(
            dest.entry(ENTRY1_ID).unwrap().icon().is_none(),
            "the current version follows the remote and has no icon"
        );
        assert_eq!(
            dest.num_custom_icons(),
            1,
            "the archived version still points at the image"
        );
    }

    /// An image nothing points at any more is dead weight in every save and
    /// in every future merge.
    #[test]
    fn pruning_drops_images_nothing_references() {
        let mut db = create_test_database();
        db.entry_mut(ENTRY1_ID)
            .unwrap()
            .set_icon_custom_new(vec![9, 9, 9]);
        db.entry_mut(ENTRY1_ID).unwrap().set_icon_none();

        assert_eq!(db.num_custom_icons(), 1, "set_icon_none leaves the image");
        db.prune_unused_custom_icons();
        assert_eq!(db.num_custom_icons(), 0, "the prune collects it");
    }

    /// Re-fetching icons in bulk replaces them one after another. Without a
    /// prune every previous image stays, so the vault grows without bound and
    /// its icon table drifts away from every other copy.
    #[test]
    fn pruning_after_replacing_an_icon_keeps_only_the_current_image() {
        let mut db = create_test_database();
        db.entry_mut(ENTRY1_ID)
            .unwrap()
            .set_icon_custom_new(vec![1, 1, 1]);
        db.entry_mut(ENTRY1_ID)
            .unwrap()
            .set_icon_custom_new(vec![2, 2, 2]);

        db.prune_unused_custom_icons();

        assert_eq!(db.num_custom_icons(), 1, "only the current image is kept");
        assert_eq!(
            db.entry(ENTRY1_ID).unwrap().custom_icon().unwrap().data,
            vec![2, 2, 2]
        );
    }

    /// The prune reads the entry and group icon fields, not the reference
    /// cache: a version archived by `track_changes` never registers itself
    /// there, and deleting its image would blank an archived icon.
    #[test]
    fn pruning_keeps_an_image_a_history_version_still_uses() {
        let mut db = create_test_database();
        db.entry_mut(ENTRY1_ID)
            .unwrap()
            .set_icon_custom_new(vec![1, 1, 1]);
        db.entry_mut(ENTRY1_ID)
            .unwrap()
            .track_changes()
            .as_mut()
            .set_icon_custom_new(vec![2, 2, 2]);

        db.prune_unused_custom_icons();

        assert_eq!(
            db.num_custom_icons(),
            2,
            "the archived version still renders its own icon"
        );
    }

    /// Build up an example database for testing
    ///
    /// The database structure is as follows:
    ///
    /// root (ROOT_GROUP_ID)
    /// ├── entry1 (ENTRY1_ID)
    /// ├── group1 (GROUP1_ID)
    /// │   └── subgroup1 (SUBGROUP1_ID)
    /// │       └── entry2 (ENTRY2_ID)
    /// └── group2 (GROUP2_ID)
    ///    └── subgroup2 (SUBGROUP2_ID)
    ///
    fn create_test_database() -> Database {
        let mut db = Database::new_with_root_id(ROOT_GROUP_ID);

        // build up root -> group1 -> subgroup1 -> entry2
        db.root_mut()
            .add_group_with_id(GROUP1_ID)
            .edit(|g| g.name = "group1".to_string())
            .add_group_with_id(SUBGROUP1_ID)
            .edit(|sg| sg.name = "subgroup1".to_string())
            .add_entry_with_id(ENTRY2_ID)
            .edit(|e| e.set_unprotected("Title", "entry2"));

        // build up root -> group2 -> subgroup2
        db.root_mut()
            .add_group_with_id(GROUP2_ID)
            .edit(|g| g.name = "group2".to_string())
            .add_group_with_id(SUBGROUP2_ID)
            .edit(|sg| sg.name = "subgroup2".to_string());

        // Placing the first entry in the root group
        db.root_mut()
            .add_entry_with_id(ENTRY1_ID)
            .edit(|e| e.set_unprotected("Title", "entry1"));

        db
    }

    /// sleep for 1 second to ensure different timestamps
    fn sleep() {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    fn assert_history_ordered(history: &History) {
        let mut last_modification_time: Option<&chrono::NaiveDateTime> = None;
        for entry in &history.entries {
            if last_modification_time.is_none() {
                last_modification_time = entry.times.last_modification.as_ref();
            }

            if let Some(entry_modification_time) = entry.times.last_modification.as_ref() {
                if last_modification_time.unwrap() < entry_modification_time {
                    panic!(
                        "History entries are not ordered by last modification time: {:?} came after {:?}",
                        last_modification_time, entry_modification_time
                    );
                }
                last_modification_time = Some(entry_modification_time);
            }
        }
    }

    /// Test that merging a database with itself results in no changes.
    #[test]
    fn test_idempotence() {
        let mut destination_db = create_test_database();
        let source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);
        assert_eq!(destination_db.root().entries().count(), 1);
        assert_eq!(destination_db.root().groups().count(), 2);

        assert_eq!(destination_db.entries.len(), entry_count_before);
        assert_eq!(destination_db.groups.len(), group_count_before);

        // The two groups should be exactly the same after merging, since
        // nothing was performed during the merge.
        assert_eq!(destination_db, source_db);

        sleep();

        // Now modify an entry in the destination database, and merge again.
        destination_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .edit_tracking(|e| e.set_unprotected("Title", "entry1_updated"));

        // Merging should ignore the change, since destination is more recent.
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);
        let destination_db_just_after_merge = destination_db.clone();

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        // Merging twice in a row, even if the first merge updated the destination group,
        // should not create more changes.
        assert_eq!(destination_db_just_after_merge, destination_db);
    }

    /// Pure UI view state (IsExpanded, LastTopVisibleEntry) is written by
    /// clients without bumping modification times — with tied timestamps it
    /// must not count as divergence and must not fail the merge.
    #[test]
    fn test_view_state_only_divergence_is_not_a_merge_error() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let dest_expanded = destination_db.groups.get(&GROUP1_ID).unwrap().is_expanded;
        source_db.groups.get_mut(&GROUP1_ID).unwrap().is_expanded = !dest_expanded;
        source_db
            .groups
            .get_mut(&GROUP2_ID)
            .unwrap()
            .last_top_visible_entry = Some(ENTRY1_ID);

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);
        // On a tie the destination keeps its own view state.
        assert_eq!(
            destination_db.groups.get(&GROUP1_ID).unwrap().is_expanded,
            dest_expanded
        );
        assert_eq!(
            destination_db
                .groups
                .get(&GROUP2_ID)
                .unwrap()
                .last_top_visible_entry,
            None
        );
    }

    /// Test that a new entry in source is added to destination when merging.
    #[test]
    fn test_add_new_entry() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // create a new entry in source_db and retain its id
        let new_entry_id = source_db
            .root_mut()
            .add_entry()
            .edit_tracking(|e| e.set_unprotected("Title", "new_entry"))
            .id();

        // merge source_db into destination_db -- this should add the new entry
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before + 1);
        assert_eq!(group_count_after, group_count_before);

        let root_entries_count = destination_db.root().entries().count();
        assert_eq!(root_entries_count, 2);

        let new_entry = destination_db
            .entry(new_entry_id)
            .expect("New entry should exist");
        assert_eq!(new_entry.get(fields::TITLE), Some("new_entry"));

        // Merging the same group again should not create a duplicate entry.
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before + 1);
        assert_eq!(group_count_after, group_count_before);

        let root_entries_count = destination_db.root().entries().count();
        assert_eq!(root_entries_count, 2);
    }

    /// Test that an entry that is marked as deleted in the destination database is not re-added
    /// when merging from source
    #[test]
    fn test_deleted_entry_in_destination() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // add a new entry in source_db that will be marked as deleted in destination_db
        let deleted_entry_id = source_db
            .root_mut()
            .add_entry()
            .edit_tracking(|e| {
                e.set_unprotected("Title", "deleted_entry");
            })
            .id();

        // mark the entry as deleted in destination_db
        destination_db
            .deleted_objects
            .insert(deleted_entry_id.uuid(), Some(Times::now()));

        // merge source_db into destination_db -- the entry should not be added
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.entry(deleted_entry_id).is_none());
    }

    /// Test that an entry that is updated and moved to a group in source but that group is deleted
    /// later in dest should cause the group to be re-added and the entry to be moved there.
    #[test]
    fn test_updated_entry_under_deleted_group() {
        let mut destination_db = create_test_database();

        let modified_entry_id = destination_db
            .root_mut()
            .add_entry()
            .edit(|e| e.set_unprotected("Title", "original_title"))
            .id();

        let deleted_group_id = destination_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "deleted_group".to_string())
            .id();

        let mut source_db = destination_db.clone();

        sleep();

        // perform the update of the entry in source_db and move it to the group that will be
        // deleted
        source_db
            .entry_mut(modified_entry_id)
            .unwrap()
            .track_changes()
            .edit(|e| {
                e.set_unprotected("Title", "modified_title");
            })
            .move_to(deleted_group_id)
            .unwrap();

        sleep();

        // delete the group in destination_db
        destination_db.group_mut(deleted_group_id).unwrap().remove();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // perform the merge - the group should be re-added and the entry moved there
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 3); // recreate group, move entry, update entry

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before + 1);

        assert!(destination_db.group(deleted_group_id).is_some());
        assert!(destination_db.entry(modified_entry_id).is_some());
    }

    /// Test that a group that is marked as deleted in the destination database is not re-added
    /// when merging from source
    #[test]
    fn test_deleted_group_in_destination() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // add a new group in source_db
        let deleted_group_id = source_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "deleted_group".to_string())
            .id();

        // mark the group as deleted in destination_db
        destination_db
            .deleted_objects
            .insert(deleted_group_id.uuid(), Some(Times::now()));

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(deleted_group_id).is_none());
    }

    /// Test that an entry that is marked as deleted in the source database is deleted from destination
    #[test]
    fn test_deleted_entry_in_source() {
        let mut destination_db = create_test_database();

        let deleted_entry_id = destination_db
            .root_mut()
            .add_entry()
            .edit_tracking(|e| e.set_unprotected("Title", "deleted_entry"))
            .id();

        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // mark the entry as deleted in source_db
        source_db
            .entry_mut(deleted_entry_id)
            .unwrap()
            .track_changes()
            .remove();

        // perform the merge - the entry should be deleted
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        // verify that the entry was deleted
        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before - 1);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.entry(deleted_entry_id).is_none());
        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_entry_id.uuid()));
    }

    /// Test that a group that is marked as deleted in the source database is deleted from destination
    #[test]
    fn test_deleted_group_in_source() {
        let mut destination_db = create_test_database();

        let deleted_group_id = destination_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "deleted_group".to_string())
            .id();

        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // mark the entry as deleted in source_db
        source_db
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();

        // perform the merge - the entry should be deleted
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        // verify that the entry was deleted
        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before - 1);

        assert!(destination_db.group(deleted_group_id).is_none());
        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_group_id.uuid()));
    }

    /// Test that an entry that is marked as deleted in the source database but modified in
    /// destination is not deleted
    #[test]
    fn test_deleted_entry_in_source_modified_in_destination() {
        let mut destination_db = create_test_database();

        let deleted_entry_id = destination_db
            .root_mut()
            .add_entry()
            .edit_tracking(|e| e.set_unprotected("Title", "deleted_entry"))
            .id();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        let mut source_db = destination_db.clone();

        // mark the entry as deleted in source_db
        source_db
            .entry_mut(deleted_entry_id)
            .unwrap()
            .track_changes()
            .remove();

        sleep();

        // modify the entry in destination_db
        destination_db
            .entry_mut(deleted_entry_id)
            .unwrap()
            .edit_tracking(|e| e.set_unprotected("Title", "modified_in_destination"));

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.entry(deleted_entry_id).is_some());
        assert!(!destination_db
            .deleted_objects
            .contains_key(&deleted_entry_id.uuid()));
    }

    /// Test that a group subtree that is marked as deleted in the source database is deleted from
    /// destination
    #[test]
    fn test_group_subtree_deletion() {
        let mut destination_db = create_test_database();

        let deleted_group_id = destination_db
            .root_mut()
            .add_group()
            .edit(|g| {
                g.name = "deleted_group".to_string();
            })
            .id();

        let deleted_subgroup_id = destination_db
            .group_mut(deleted_group_id)
            .unwrap()
            .add_group()
            .edit(|g| {
                g.name = "deleted_subgroup".to_string();
            })
            .id();

        let deleted_entry_id = destination_db
            .group_mut(deleted_subgroup_id)
            .unwrap()
            .add_entry()
            .edit_tracking(|e| {
                e.set_unprotected("Title", "deleted_entry");
            })
            .id();

        let mut source_db = destination_db.clone();

        // mark the entire group subtree as deleted in source_db
        source_db
            .root_mut()
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // perform the merge - the entire subtree should be deleted
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 3);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before - 1);
        assert_eq!(group_count_after, group_count_before - 2);

        assert!(destination_db.entry(deleted_entry_id).is_none());
        assert!(destination_db.group(deleted_subgroup_id).is_none());
        assert!(destination_db.group(deleted_group_id).is_none());

        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_entry_id.uuid()));
        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_subgroup_id.uuid()));
        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_group_id.uuid()));
    }

    /// Test that a tree that was deleted in source, but contains a group that is newer in
    /// destination is only partially deleted.
    #[test]
    fn test_group_subtree_partial_deletion() {
        let mut destination_db = create_test_database();

        let deleted_group_id = destination_db
            .root_mut()
            .add_group()
            .edit(|g| {
                g.name = "deleted_group".to_string();
            })
            .id();

        let deleted_subgroup_id = destination_db
            .group_mut(deleted_group_id)
            .unwrap()
            .add_group()
            .edit(|g| {
                g.name = "deleted_subgroup".to_string();
            })
            .id();

        let deleted_entry_id = destination_db
            .group_mut(deleted_subgroup_id)
            .unwrap()
            .add_entry()
            .edit(|e| {
                e.set_unprotected("Title", "deleted_entry");
            })
            .id();

        let mut source_db = destination_db.clone();

        sleep();

        // mark the entire group subtree as deleted in source_db
        source_db
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();

        sleep();

        // modify the deleted subgroup in destination_db to be newer than the deletion time
        destination_db
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .edit(|g| {
                g.notes = Some("modified in destination".to_string());
            });

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // perform the merge - the entry and subgroup should be deleted, but the group should
        // remain
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 2);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before - 1);
        assert_eq!(group_count_after, group_count_before - 1);

        assert!(destination_db.entry(deleted_entry_id).is_none());
        assert!(destination_db.group(deleted_subgroup_id).is_none());
        assert!(destination_db.group(deleted_group_id).is_some());

        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_entry_id.uuid()));
        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_subgroup_id.uuid()));
        assert!(!destination_db
            .deleted_objects
            .contains_key(&deleted_group_id.uuid()));
    }

    /// Test that a group that is marked as deleted in the source database but modified in
    /// destination is not deleted
    #[test]
    fn test_deleted_group_in_source_modified_in_destination() {
        let mut destination_db = create_test_database();

        let deleted_group_id = destination_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "deleted_group".to_string())
            .id();

        let mut source_db = destination_db.clone();

        // mark the group as deleted in source_db
        source_db
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();

        sleep();

        // modify the group in destination_db
        destination_db
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .edit(|g| g.notes = Some("modified_in_destination".to_string()));

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // perform the merge - the group should not be deleted
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(deleted_group_id).is_some());

        assert!(!destination_db
            .deleted_objects
            .contains_key(&deleted_group_id.uuid()));
    }

    /// Test that a group that is marked as deleted in the source database but has new entries
    /// added in destination is not deleted
    #[test]
    fn test_deleted_group_has_new_entries() {
        let mut destination_db = create_test_database();

        let deleted_group_id = destination_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "deleted_group".to_string())
            .id();

        let mut source_db = destination_db.clone();

        // mark the group as deleted in source_db
        source_db
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();

        sleep();

        // add a new entry to the deleted group in destination_db
        let new_entry_id = destination_db
            .group_mut(deleted_group_id)
            .unwrap()
            .add_entry()
            .edit_tracking(|e| {
                e.set_unprotected("Title", "new_entry_in_deleted_group");
            })
            .id();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // perform the merge - the group should not be deleted
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(deleted_group_id).is_some());
        assert!(destination_db.entry(new_entry_id).is_some());

        assert!(!destination_db
            .deleted_objects
            .contains_key(&deleted_group_id.uuid()));
        assert!(!destination_db.deleted_objects.contains_key(&new_entry_id.uuid()));
    }

    /// Test that a new entry in a non-root group in source is added to destination when merging.
    #[test]
    fn test_add_new_non_root_entry() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        let new_entry_id = source_db
            .group_mut(GROUP1_ID)
            .unwrap()
            .add_entry()
            .edit_tracking(|e| {
                e.set_unprotected("Title", "new_entry");
            })
            .id();

        // perform the merge - this should add the new entry
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before + 1);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.entry(new_entry_id).is_some());
    }

    // Test that a new entry in source under a new group/subgroup is added to destination when
    // merging.
    #[test]
    fn test_add_new_entry_new_group() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        let new_group_id = source_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "new_group".to_string())
            .id();

        let new_subgroup_id = source_db
            .group_mut(new_group_id)
            .unwrap()
            .add_group()
            .edit(|g| g.name = "new_subgroup".to_string())
            .id();

        let new_entry_id = source_db
            .group_mut(new_subgroup_id)
            .unwrap()
            .add_entry()
            .edit_tracking(|e| {
                e.set_unprotected("Title", "new_entry");
            })
            .id();

        // perform the merge - this should add the new entry along with the new group and subgroup
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 3);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before + 1);
        assert_eq!(group_count_after, group_count_before + 2);

        assert!(destination_db.group(new_group_id).is_some());
        assert!(destination_db.group(new_subgroup_id).is_some());
        assert!(destination_db.entry(new_entry_id).is_some());
    }

    /// Test that an entry is relocated from one group to another in source and the relocation
    /// is reflected in destination when merging.
    #[test]
    fn test_entry_relocation_existing_group() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // before
        // root (ROOT_GROUP_ID)
        // ├── entry1 (ENTRY1_ID)
        // ├── group1 (GROUP1_ID)
        // │   └── subgroup1 (SUBGROUP1_ID)
        // │       └── entry2 (ENTRY2_ID)   <-- this entry
        // └── group2 (GROUP2_ID)
        //    └── subgroup2 (SUBGROUP2_ID)
        //
        // after
        // root (ROOT_GROUP_ID)
        // ├── entry1 (ENTRY1_ID)
        // ├── group1 (GROUP1_ID)
        // │   └── subgroup1 (SUBGROUP1_ID)
        // └── group2 (GROUP2_ID)
        //     ├── entry2 (ENTRY2_ID)   <-- moved here
        //     └── subgroup2 (SUBGROUP2_ID)
        //
        source_db
            .entry_mut(ENTRY2_ID)
            .unwrap()
            .track_changes()
            .move_to(GROUP2_ID)
            .expect("move successful");

        let location_changed_timestamp = source_db
            .entry(ENTRY2_ID)
            .unwrap()
            .times
            .location_changed
            .unwrap();

        // perform the merge - this should relocate the entry in destination_db
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(group_count_after, group_count_before);
        assert_eq!(entry_count_after, entry_count_before);

        assert!(destination_db.entry(ENTRY2_ID).is_some());

        let entry = destination_db.entry(ENTRY2_ID).unwrap();
        assert_eq!(entry.parent().id(), GROUP2_ID);
        assert_eq!(entry.times.location_changed, Some(location_changed_timestamp));
    }

    /// Test that an entry is relocated in source and modified in both source and destination
    /// and the correct content is kept after merging.
    #[test]
    fn test_entry_relocation_and_update() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // perform first edit of entry in source
        source_db.entry_mut(ENTRY2_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "entry2_modified_in_source");
        });

        // relocate entry in source
        source_db
            .entry_mut(ENTRY2_ID)
            .unwrap()
            .track_changes()
            .move_to(GROUP2_ID)
            .expect("move successful");

        let location_changed_timestamp = source_db
            .entry(ENTRY2_ID)
            .unwrap()
            .times
            .location_changed
            .unwrap();

        sleep();

        // perform second edit of entry in destination
        destination_db.entry_mut(ENTRY2_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "entry2_modified_in_destination");
        });

        let entry_modified_timestamp = destination_db
            .entry(ENTRY2_ID)
            .unwrap()
            .times
            .last_modification
            .unwrap();

        // perform the merge - this should relocate the entry in destination_db and keep the
        // content from destination_db since it was modified later
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(group_count_after, group_count_before);
        assert_eq!(entry_count_after, entry_count_before);

        // check that move occurred
        assert!(destination_db.entry(ENTRY2_ID).is_some());
        let entry = destination_db.entry(ENTRY2_ID).unwrap();
        assert_eq!(entry.parent().id(), GROUP2_ID);
        assert_eq!(entry.times.location_changed, Some(location_changed_timestamp));

        // check that content from destination is kept
        assert_eq!(entry.get(fields::TITLE), Some("entry2_modified_in_destination"));
        assert_eq!(entry.times.last_modification, Some(entry_modified_timestamp));
    }

    /// Test that if an entry is moved in source and modified in destination, the entry stays
    /// in the new location and gets the modifications.
    #[test]
    fn test_entry_relocation_in_destination_and_update() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // edit entry in source
        source_db.entry_mut(ENTRY2_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected(fields::TITLE, "entry2_modified_in_source");
        });

        let entry_modified_timestamp = source_db
            .entry(ENTRY2_ID)
            .unwrap()
            .times
            .last_modification
            .unwrap();

        // relocate entry in destination
        destination_db
            .entry_mut(ENTRY2_ID)
            .unwrap()
            .track_changes()
            .move_to(GROUP2_ID)
            .expect("move successful");

        let location_changed_timestamp = destination_db
            .entry(ENTRY2_ID)
            .unwrap()
            .times
            .location_changed
            .unwrap();

        // perform the merge - this should keep the location from destination and the content from
        // source
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(group_count_after, group_count_before);
        assert_eq!(entry_count_after, entry_count_before);

        // check that move occurred
        assert!(destination_db.entry(ENTRY2_ID).is_some());

        let entry = destination_db.entry(ENTRY2_ID).unwrap();
        assert_eq!(entry.parent().id(), GROUP2_ID);
        assert_eq!(entry.times.location_changed, Some(location_changed_timestamp));

        // check that content from source is kept
        assert_eq!(entry.get(fields::TITLE), Some("entry2_modified_in_source"));
        assert_eq!(entry.times.last_modification, Some(entry_modified_timestamp));
    }

    /// Test that an entry can be relocated into a newly created group
    #[test]
    fn test_entry_relocation_new_group() {
        let mut destination_db = create_test_database();

        let new_entry_id = destination_db
            .root_mut()
            .add_entry()
            .edit(|e| {
                e.set_unprotected("Title", "new_entry");
            })
            .id();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        let mut source_db = destination_db.clone();

        let new_group_id = source_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "new_group".to_string())
            .id();

        sleep();

        // modify the entry in source
        source_db.entry_mut(new_entry_id).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "new_entry_modified_in_source");
        });

        // relocate the entry to the new group in source
        source_db
            .entry_mut(new_entry_id)
            .unwrap()
            .track_changes()
            .move_to(new_group_id)
            .expect("move successful");

        // perform the merge - this should create the new group and update and relocate the entry there
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 3);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before + 1);

        assert!(destination_db.entry(new_entry_id).is_some());
        let entry = destination_db.entry(new_entry_id).unwrap();
        assert_eq!(entry.parent().id(), new_group_id);
        assert_eq!(entry.get(fields::TITLE), Some("new_entry_modified_in_source"));
    }

    /// Test that a group relocation in source is reflected in destination when merging.
    #[test]
    fn test_group_relocation() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // before
        // root (ROOT_GROUP_ID)
        // ├── entry1 (ENTRY1_ID)
        // ├── group1 (GROUP1_ID)
        // │   └── subgroup1 (SUBGROUP1_ID) <-- this group
        // │       └── entry2 (ENTRY2_ID)
        // └── group2 (GROUP2_ID)
        //    └── subgroup2 (SUBGROUP2_ID)
        //
        // after
        // root (ROOT_GROUP_ID)
        // ├── entry1 (ENTRY1_ID)
        // ├── group1 (GROUP1_ID)
        // └── group2 (GROUP2_ID)
        //    └── subgroup2 (SUBGROUP2_ID)
        //        └── subgroup1 (SUBGROUP1_ID) <-- moved here
        //            └── entry2 (ENTRY2_ID)

        source_db
            .group_mut(SUBGROUP1_ID)
            .unwrap()
            .track_changes()
            .move_to(GROUP2_ID)
            .expect("move successful");

        let location_changed_timestamp = source_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .location_changed
            .unwrap();

        // perform the merge - this should relocate the group in destination_db
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(SUBGROUP1_ID).is_some());
        assert!(destination_db.entry(ENTRY2_ID).is_some());

        let group = destination_db.group(SUBGROUP1_ID).unwrap();
        assert_eq!(group.parent().unwrap().id(), GROUP2_ID);
        assert_eq!(group.times.location_changed, Some(location_changed_timestamp));
    }

    /// Test that an entry updated in destination is not touched when merging.
    #[test]
    fn test_update_in_destination_no_conflict() {
        let mut destination_db = create_test_database();
        let source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // update entry in destination
        destination_db.entry_mut(ENTRY1_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "entry1_updated");
        });

        // perform the merge - this should not change anything since source is older
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        // check that history is preserved
        let merged_history = destination_db.entry(ENTRY1_ID).unwrap().history.clone().unwrap();
        assert_history_ordered(&merged_history);
        assert_eq!(merged_history.entries.len(), 1);

        // check that we can find the old version of the entry
        let merged_entry = &merged_history.entries[0];
        assert_eq!(merged_entry.get(fields::TITLE), Some("entry1"));

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert_eq!(
            destination_db.entry(ENTRY1_ID).unwrap().get(fields::TITLE),
            Some("entry1_updated")
        );
    }

    /// Test that an entry updated in source is merged into destination when merging.
    #[test]
    fn test_update_in_source_no_conflict() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // update entry in source
        source_db.entry_mut(ENTRY1_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "entry1_updated");
        });

        // perform the merge - this should update the entry in destination_db
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        // check that history is preserved
        let merged_history = destination_db.entry(ENTRY1_ID).unwrap().history.clone().unwrap();
        assert_history_ordered(&merged_history);
        assert_eq!(merged_history.entries.len(), 1);

        // check that we can find the old version of the entry
        let merged_entry = &merged_history.entries[0];
        assert_eq!(merged_entry.get(fields::TITLE), Some("entry1"));

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        // check that the entry was updated
        assert_eq!(
            destination_db.entry(ENTRY1_ID).unwrap().get(fields::TITLE),
            Some("entry1_updated")
        );
    }

    /// Test that an entry updated in both source and destination is merged correctly.
    #[test]
    fn test_update_with_conflicts() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // update entry in destination
        destination_db.entry_mut(ENTRY1_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "entry1_updated_from_destination");
        });

        sleep();

        // update entry in source
        source_db.entry_mut(ENTRY1_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "entry1_updated_from_source");
        });

        // perform the merge - this should merge the changes from both databases, keeping the newer
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        // check that the entry was updated with the source change (newer)
        let entry = destination_db.entry(ENTRY1_ID).unwrap();
        assert_eq!(entry.get(fields::TITLE), Some("entry1_updated_from_source"));

        // check that history is preserved and contains both older versions
        let merged_history = entry.history.clone().unwrap();
        assert_history_ordered(&merged_history);
        assert_eq!(merged_history.entries.len(), 2);
        assert_eq!(
            merged_history.entries[0].get(fields::TITLE),
            Some("entry1_updated_from_destination")
        );
        assert_eq!(merged_history.entries[1].get(fields::TITLE), Some("entry1"));

        // Merging again should not result in any additional change.
        let merge_result = destination_db.merge(&destination_db.clone()).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);
    }

    /// Test that a group updated in source is merged into destination when merging.
    #[test]
    fn test_group_update_in_source() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        source_db.group_mut(SUBGROUP1_ID).unwrap().edit_tracking(|g| {
            g.name = "subgroup1_updated_name".to_string();
        });

        let modification_timestamp = source_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .last_modification
            .unwrap();

        // perform the merge - this should update the group in destination
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(SUBGROUP1_ID).is_some());

        assert_eq!(
            destination_db.group(SUBGROUP1_ID).unwrap().name,
            "subgroup1_updated_name"
        );
        assert_eq!(
            destination_db
                .group(SUBGROUP1_ID)
                .unwrap()
                .times
                .last_modification,
            Some(modification_timestamp)
        );
    }

    /// Test that a group updated in destination is not changed when merging.
    #[test]
    fn test_group_update_in_destination() {
        let mut destination_db = create_test_database();
        let source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        destination_db
            .group_mut(SUBGROUP1_ID)
            .unwrap()
            .edit_tracking(|g| {
                g.name = "subgroup1_updated_name".to_string();
            });

        let last_modification = destination_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .last_modification
            .unwrap();

        // perform the merge - this should not change anything since source is older
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(SUBGROUP1_ID).is_some());
        assert_eq!(
            destination_db.group(SUBGROUP1_ID).unwrap().name,
            "subgroup1_updated_name"
        );

        assert_eq!(
            destination_db
                .group(SUBGROUP1_ID)
                .unwrap()
                .times
                .last_modification,
            Some(last_modification)
        );
    }

    /// Test that a group updated in source and relocated is merged correctly.
    #[test]
    fn test_group_update_and_relocation() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        source_db
            .group_mut(SUBGROUP1_ID)
            .unwrap()
            .track_changes()
            .edit(|g| {
                g.name = "subgroup1_updated_name".to_string();
            })
            .move_to(GROUP2_ID)
            .expect("move successful");

        let modification_timestamp = source_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .last_modification
            .unwrap();

        let location_changed_timestamp = source_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .location_changed
            .unwrap();

        // perform the merge - this should update and relocate the group in destination
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 2);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(SUBGROUP1_ID).is_some());
        let group = destination_db.group(SUBGROUP1_ID).unwrap();
        assert_eq!(group.name, "subgroup1_updated_name");
        assert_eq!(group.parent().unwrap().id(), GROUP2_ID);
        assert_eq!(group.times.last_modification, Some(modification_timestamp));
        assert_eq!(group.times.location_changed, Some(location_changed_timestamp));
    }

    /// Test that a group updated in source and relocated in destionation is merged correctly.
    #[test]
    fn test_group_update_in_destination_and_relocation_in_source() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // rename group in source
        source_db.group_mut(SUBGROUP1_ID).unwrap().edit_tracking(|g| {
            g.name = "subgroup1_updated_name".to_string();
        });

        let modification_timestamp = source_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .last_modification
            .unwrap();

        // relocate group in destination
        destination_db
            .group_mut(SUBGROUP1_ID)
            .unwrap()
            .track_changes()
            .move_to(GROUP2_ID)
            .expect("move successful");

        let location_changed_timestamp = destination_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .location_changed
            .unwrap();

        // perform the merge - this should update the group name from source and keep the new
        // location from destination
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(SUBGROUP1_ID).is_some());
        let group = destination_db.group(SUBGROUP1_ID).unwrap();
        assert_eq!(group.name, "subgroup1_updated_name");
        assert_eq!(group.parent().unwrap().id(), GROUP2_ID);
        assert_eq!(group.times.last_modification, Some(modification_timestamp));
        assert_eq!(group.times.location_changed, Some(location_changed_timestamp));
    }

    #[test]
    fn test_merge_untracked_group_history() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        // this is an invalid edit as the last modified timestamp of the group is not updated
        source_db
            .group_mut(GROUP1_ID)
            .unwrap()
            .edit(|g| {
                g.name = "group1_updated_name".to_string();
            })
            .move_to(GROUP2_ID)
            .expect("move successful");

        assert_eq!(
            destination_db.group(GROUP1_ID).unwrap().times,
            source_db.group(GROUP1_ID).unwrap().times
        );

        // there will be an error during merge since the edit in source_db is not tracked and has
        // the same timestamp as the group in destination_db
        assert!(destination_db.merge(&source_db).is_err());

        // remove the timestamps to test warnings
        destination_db
            .group_mut(GROUP1_ID)
            .unwrap()
            .times
            .last_modification = None;
        destination_db
            .group_mut(GROUP1_ID)
            .unwrap()
            .times
            .location_changed = None;
        source_db.group_mut(GROUP1_ID).unwrap().times.last_modification = None;
        source_db.group_mut(GROUP1_ID).unwrap().times.location_changed = None;

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 3);
        assert_eq!(merge_result.events.len(), 0);
    }

    #[test]
    fn test_merge_untracked_entry_history() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        // this is an invalid edit as the last modified timestamp of the entry is not updated
        source_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .edit(|e| {
                e.set_unprotected("Title", "entry1_updated_title");
            })
            .move_to(GROUP2_ID)
            .expect("move successful");

        assert_eq!(
            destination_db.entry(ENTRY1_ID).unwrap().times,
            source_db.entry(ENTRY1_ID).unwrap().times
        );

        // there will be an error during merge since the edit in source_db is not tracked and has
        // the same timestamp as the entry in destination_db
        assert!(destination_db.merge(&source_db).is_err());

        // remove the timestamps to test warnings
        destination_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .times
            .last_modification = None;
        destination_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .times
            .location_changed = None;
        source_db.entry_mut(ENTRY1_ID).unwrap().times.last_modification = None;
        source_db.entry_mut(ENTRY1_ID).unwrap().times.location_changed = None;

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 3);
        assert_eq!(merge_result.events.len(), 0);
    }

    #[test]
    fn merge_imports_attachment_on_new_entry() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();
        let entry_id = source_db.root_mut().add_entry().id();
        source_db
            .entry_mut(entry_id)
            .unwrap()
            .add_attachment("secret.bin", Value::protected(vec![1, 2, 3]));

        destination_db.merge(&source_db).unwrap();

        let entry = destination_db.entry(entry_id).unwrap();
        let attachment = entry.attachment_by_name("secret.bin").unwrap();
        assert_eq!(attachment.data.get(), &[1, 2, 3]);
        assert!(attachment.data.is_protected());
        assert_eq!(destination_db.num_attachments(), 1);
    }

    #[test]
    fn merge_remaps_colliding_attachment_ids() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        sleep();
        destination_db
            .entry_mut(ENTRY2_ID)
            .unwrap()
            .add_attachment("local.bin", Value::unprotected(vec![4, 5, 6]));
        destination_db
            .entry_mut(ENTRY2_ID)
            .unwrap()
            .times
            .last_modification = Some(Times::now() + chrono::Duration::seconds(60));
        sleep();
        source_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .add_attachment("remote.bin", Value::unprotected(vec![7, 8, 9]));
        source_db.entry_mut(ENTRY1_ID).unwrap().times.last_modification =
            Some(Times::now() + chrono::Duration::seconds(60));

        destination_db.merge(&source_db).unwrap();

        assert_eq!(destination_db.num_attachments(), 2);
        assert_eq!(
            destination_db
                .entry(ENTRY2_ID)
                .unwrap()
                .attachment_by_name("local.bin")
                .unwrap()
                .data
                .get(),
            &[4, 5, 6]
        );
        assert_eq!(
            destination_db
                .entry(ENTRY1_ID)
                .unwrap()
                .attachment_by_name("remote.bin")
                .unwrap()
                .data
                .get(),
            &[7, 8, 9]
        );
    }

    #[test]
    fn merge_preserves_replaced_attachment_in_history() {
        let mut destination_db = create_test_database();
        destination_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .add_attachment("old.bin", Value::unprotected(vec![1, 1, 1]));
        let mut source_db = destination_db.clone();

        sleep();
        {
            let mut source_entry = source_db.entry_mut(ENTRY1_ID).unwrap();
            source_entry.remove_attachment_by_name("old.bin");
            source_entry.add_attachment("new.bin", Value::unprotected(vec![2, 2, 2]));
            source_entry.times.last_modification = Some(Times::now() + chrono::Duration::seconds(60));
        }

        destination_db.merge(&source_db).unwrap();

        let entry = destination_db.entry(ENTRY1_ID).unwrap();
        assert!(entry.attachment_by_name("old.bin").is_none());
        assert_eq!(
            entry.attachment_by_name("new.bin").unwrap().data.get(),
            &[2, 2, 2]
        );
        let old_version = entry
            .history
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .find(|historical| historical.attachments.contains_key("old.bin"))
            .expect("destination attachment must survive in history");
        let old_id = old_version.attachments["old.bin"];
        assert_eq!(destination_db.attachment(old_id).unwrap().data.get(), &[1, 1, 1]);
    }

    #[test]
    fn merge_reuses_equal_attachment_values() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();
        sleep();
        destination_db
            .entry_mut(ENTRY2_ID)
            .unwrap()
            .add_attachment("existing.bin", Value::protected(vec![4, 2]));
        destination_db
            .entry_mut(ENTRY2_ID)
            .unwrap()
            .times
            .last_modification = Some(Times::now() + chrono::Duration::seconds(60));

        sleep();
        source_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .add_attachment("same-data.bin", Value::protected(vec![4, 2]));
        source_db.entry_mut(ENTRY1_ID).unwrap().times.last_modification =
            Some(Times::now() + chrono::Duration::seconds(60));

        destination_db.merge(&source_db).unwrap();

        assert_eq!(destination_db.num_attachments(), 1);
        let first = destination_db
            .entry(ENTRY1_ID)
            .unwrap()
            .attachment_by_name("same-data.bin")
            .unwrap()
            .id();
        let second = destination_db
            .entry(ENTRY2_ID)
            .unwrap()
            .attachment_by_name("existing.bin")
            .unwrap()
            .id();
        assert_eq!(first, second);
        assert_eq!(
            destination_db.attachment(first).unwrap().entries(false).count(),
            2
        );
    }

    #[test]
    fn merge_rejects_dangling_source_attachment_reference() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();
        let entry_id = source_db.root_mut().add_entry().id();
        source_db
            .entries
            .get_mut(&entry_id)
            .unwrap()
            .attachments
            .insert("missing.bin".into(), AttachmentId::new(99));

        let error = destination_db.merge(&source_db).unwrap_err();
        assert!(matches!(error, super::MergeError::AttachmentNotFound(_)));
    }
}
