use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use mdbase::expressions::evaluator::ResolvedFileData;
use mdbase::types::schema::TypeDef;
use mdbase::Collection;
use serde_json::{json, Map, Value};

use crate::config::{load_effective_config, ArchiveConfig, EffectiveConfig};
use crate::create_compat::create_task_with_compat;
use crate::date::{apply_day_offset, get_date_part, is_before_date_safe, today_local};
use crate::field_mapping::{
    default_completed_status, denormalize_frontmatter, field_mapping_from_config,
    is_completed_status, normalize_frontmatter, resolve_display_title, FieldMapping,
};
use crate::recurrence::{
    complete as complete_recurring, recalculate as recalculate_recurring,
    uncomplete_instance as uncomplete_recurring_instance, RecurrenceInput,
};
use crate::task_ops::{
    apply_title_update, clear_archive_markers, complete_nonrecurring, ensure_archive_markers,
    ensure_delete_allowed, is_archived, uncomplete_nonrecurring,
};
use crate::time_tracking;
use crate::view_query::ViewEvalSupport;

#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub path: String,
    pub title: String,
    pub status: String,
    pub priority: Option<String>,
    pub due: Option<String>,
    pub scheduled: Option<String>,
    pub time_entries: Vec<Value>,
    pub has_active_time_entry: bool,
    pub body: String,
    pub normalized_frontmatter: Map<String, Value>,
    pub raw_frontmatter: Map<String, Value>,
}

#[derive(Debug, Clone, Default)]
pub struct TaskDraft {
    pub title: String,
    pub details: String,
    pub due: Option<String>,
    pub scheduled: Option<String>,
    pub priority: Option<String>,
    pub status: Option<String>,
    pub recurrence: Option<String>,
    pub recurrence_anchor: Option<String>,
    pub projects: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskFilter {
    All,
    Open,
    Today,
    Overdue,
    Tracked,
}

pub struct TaskRepository {
    root: PathBuf,
    pub config: EffectiveConfig,
    pub field_mapping: FieldMapping,
}

impl TaskRepository {
    pub fn open(root: impl AsRef<Path>) -> anyhow::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let config = load_effective_config(&root);
        let _collection = Collection::open(&root)
            .map_err(|error| anyhow!("failed to open mdbase collection: {}", error))?;
        let field_mapping = field_mapping_from_config(&config);
        Ok(Self {
            root,
            config,
            field_mapping,
        })
    }

    fn collection(&self) -> anyhow::Result<Collection> {
        Collection::open(&self.root)
            .map_err(|error| anyhow!("failed to open mdbase collection: {}", error))
    }

    pub fn build_view_eval_support(&self) -> anyhow::Result<ViewEvalSupport> {
        let collection = self.collection()?;
        Ok(ViewEvalSupport::build(&collection))
    }

    pub fn absolute_task_path(&self, task: &TaskRecord) -> PathBuf {
        self.root.join(&task.path)
    }

    pub fn list_tasks(
        &self,
        filter: TaskFilter,
        reference_date: &str,
    ) -> anyhow::Result<Vec<TaskRecord>> {
        let collection = self.collection()?;
        let query = collection.query(&json!({
            "query": {
                "types": ["task"],
                "include_body": true,
            }
        }));
        let results = query
            .get("results")
            .and_then(Value::as_array)
            .context("mdbase query did not return results")?;

        let mut tasks = Vec::new();
        for entry in results {
            let path = entry
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let raw = entry
                .get("frontmatter")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let body = entry
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let normalized = normalize_frontmatter(&raw, &self.field_mapping);
            let title = resolve_display_title(&normalized, &self.field_mapping, Some(&path))
                .unwrap_or_else(|| path.clone());
            let status = normalized
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or(&self.config.defaults.status)
                .to_string();
            let task = TaskRecord {
                path,
                title,
                status,
                priority: normalized
                    .get("priority")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                due: normalized
                    .get("due")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                scheduled: normalized
                    .get("scheduled")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                time_entries: normalized
                    .get("timeEntries")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                has_active_time_entry: normalized
                    .get("timeEntries")
                    .and_then(Value::as_array)
                    .and_then(|entries| time_tracking::has_active_entry(entries).ok())
                    .unwrap_or(false),
                body,
                normalized_frontmatter: normalized,
                raw_frontmatter: raw,
            };
            if matches_filter(
                &task,
                filter,
                &self.field_mapping,
                &self.config.archive,
                reference_date,
            ) {
                tasks.push(task);
            }
        }

        tasks.sort_by(task_sort_key);
        Ok(tasks)
    }

    pub fn search_tasks(
        &self,
        filter: TaskFilter,
        query: Option<&str>,
        reference_date: &str,
    ) -> anyhow::Result<Vec<TaskRecord>> {
        let tasks = self.list_tasks(filter, reference_date)?;
        let Some(query) = query.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(tasks);
        };
        let needle = query.to_ascii_lowercase();
        Ok(tasks
            .into_iter()
            .filter(|task| {
                task.title.to_ascii_lowercase().contains(&needle)
                    || task.path.to_ascii_lowercase().contains(&needle)
                    || task.body.to_ascii_lowercase().contains(&needle)
                    || task
                        .priority
                        .as_deref()
                        .map(|value| value.to_ascii_lowercase().contains(&needle))
                        .unwrap_or(false)
            })
            .collect())
    }

    pub fn read_task(&self, path: &str) -> anyhow::Result<TaskRecord> {
        let collection = self.collection()?;
        let result = collection.read(&json!({ "path": path }));
        let raw = result
            .get("frontmatter")
            .and_then(Value::as_object)
            .cloned()
            .context("read missing frontmatter")?;
        let body = result
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let normalized = normalize_frontmatter(&raw, &self.field_mapping);
        Ok(TaskRecord {
            path: path.to_string(),
            title: resolve_display_title(&normalized, &self.field_mapping, Some(path))
                .unwrap_or_else(|| path.to_string()),
            status: normalized
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or(&self.config.defaults.status)
                .to_string(),
            priority: normalized
                .get("priority")
                .and_then(Value::as_str)
                .map(str::to_string),
            due: normalized
                .get("due")
                .and_then(Value::as_str)
                .map(str::to_string),
            scheduled: normalized
                .get("scheduled")
                .and_then(Value::as_str)
                .map(str::to_string),
            time_entries: normalized
                .get("timeEntries")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            has_active_time_entry: normalized
                .get("timeEntries")
                .and_then(Value::as_array)
                .and_then(|entries| time_tracking::has_active_entry(entries).ok())
                .unwrap_or(false),
            body,
            normalized_frontmatter: normalized,
            raw_frontmatter: raw,
        })
    }

    pub fn toggle_time_tracking(&self, task: &TaskRecord) -> anyhow::Result<TaskRecord> {
        let mut normalized = self.read_task(&task.path)?.normalized_frontmatter;
        let entries = normalized
            .get("timeEntries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let result = if time_tracking::has_active_entry(&entries).unwrap_or(false) {
            time_tracking::stop(&entries, None)?
        } else {
            time_tracking::start(&entries, None)?
        };
        if let Some(entries) = result.get("value") {
            normalized.insert("timeEntries".into(), entries.clone());
        }
        if let Some(date_modified) = result.get("dateModified") {
            normalized.insert("dateModified".into(), date_modified.clone());
        }
        self.write_task_update(task, normalized)
    }

    pub fn toggle_complete(&self, task: &TaskRecord) -> anyhow::Result<TaskRecord> {
        let collection = self.collection()?;
        let mut normalized = task.normalized_frontmatter.clone();
        let is_recurring = normalized
            .get("recurrence")
            .and_then(Value::as_str)
            .is_some();
        let today = today_local();

        if is_recurring {
            let input = RecurrenceInput {
                recurrence: normalized
                    .get("recurrence")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                recurrence_anchor: normalized
                    .get("recurrenceAnchor")
                    .and_then(Value::as_str)
                    .unwrap_or("scheduled")
                    .to_string(),
                scheduled: normalized
                    .get("scheduled")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                due: normalized
                    .get("due")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                date_created: normalized
                    .get("dateCreated")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                complete_instances: normalized
                    .get("completeInstances")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                skipped_instances: normalized
                    .get("skippedInstances")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            let already_completed_today = input
                .complete_instances
                .iter()
                .any(|value| get_date_part(value) == today);
            if already_completed_today {
                let completion =
                    uncomplete_recurring_instance(&input.complete_instances, today.as_str());
                let complete_instances = completion
                    .get("completeInstances")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                normalized.insert("completeInstances".into(), Value::Array(complete_instances));

                let reference_date = apply_day_offset(&today, -1).unwrap_or_else(|| today.clone());
                let recalculated = recalculate_recurring(
                    &RecurrenceInput {
                        complete_instances: normalized
                            .get("completeInstances")
                            .and_then(Value::as_array)
                            .map(|values| {
                                values
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        ..input
                    },
                    reference_date.as_str(),
                )?;
                if let Some(value) = recalculated.get("nextScheduled") {
                    normalized.insert("scheduled".into(), value.clone());
                }
                if let Some(value) = recalculated.get("nextDue") {
                    normalized.insert("due".into(), value.clone());
                }
                if let Some(value) = recalculated.get("updatedRecurrence") {
                    normalized.insert("recurrence".into(), value.clone());
                }
            } else {
                let completion = complete_recurring(&input, today.as_str())?;
                if let Some(value) = completion.get("completeInstances") {
                    normalized.insert("completeInstances".into(), value.clone());
                }
                if let Some(value) = completion.get("skippedInstances") {
                    normalized.insert("skippedInstances".into(), value.clone());
                }
                if let Some(value) = completion.get("nextScheduled") {
                    normalized.insert("scheduled".into(), value.clone());
                }
                if let Some(value) = completion.get("nextDue") {
                    normalized.insert("due".into(), value.clone());
                }
                if let Some(value) = completion.get("updatedRecurrence") {
                    normalized.insert("recurrence".into(), value.clone());
                }
            }
        } else if is_completed_status(&self.field_mapping, Some(&task.status)) {
            normalized = uncomplete_nonrecurring(&normalized, &self.config.defaults.status, true);
        } else {
            normalized = complete_nonrecurring(
                &normalized,
                &default_completed_status(&self.field_mapping),
                Some(today.as_str()),
            )?;
        }

        normalized.insert(
            "dateModified".into(),
            Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        );
        let fields = denormalize_frontmatter(&normalized, &self.field_mapping);
        let result = collection.update(&json!({ "path": task.path, "fields": fields }));
        if let Some(error) = result.get("error") {
            return Err(anyhow!("update failed: {}", error));
        }
        self.read_task(&task.path)
    }

    pub fn toggle_skip_today(&self, task: &TaskRecord) -> anyhow::Result<TaskRecord> {
        let mut normalized = task.normalized_frontmatter.clone();
        let is_recurring = normalized
            .get("recurrence")
            .and_then(Value::as_str)
            .is_some();
        anyhow::ensure!(is_recurring, "task is not recurring");

        let today = today_local();
        let mut skipped_instances: Vec<String> = normalized
            .get("skippedInstances")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if skipped_instances
            .iter()
            .any(|value| get_date_part(value) == today)
        {
            skipped_instances.retain(|value| get_date_part(value) != today);
        } else {
            skipped_instances.push(today.clone());
            if let Some(completed) = normalized
                .get_mut("completeInstances")
                .and_then(Value::as_array_mut)
            {
                completed
                    .retain(|value| value.as_str().map(get_date_part).unwrap_or_default() != today);
            }
        }
        normalized.insert(
            "skippedInstances".into(),
            Value::Array(
                skipped_instances
                    .into_iter()
                    .map(Value::String)
                    .collect::<Vec<_>>(),
            ),
        );
        self.write_task_update(task, normalized)
    }

    pub fn create_task(&self, title: &str) -> anyhow::Result<TaskRecord> {
        self.create_task_from_draft(&TaskDraft {
            title: title.to_string(),
            ..TaskDraft::default()
        })
    }

    pub fn create_task_from_draft(&self, draft: &TaskDraft) -> anyhow::Result<TaskRecord> {
        let collection = self.collection()?;
        anyhow::ensure!(!draft.title.trim().is_empty(), "title must not be empty");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut fields = Map::new();
        fields.insert(
            self.config
                .mapping
                .get("title")
                .cloned()
                .unwrap_or_else(|| "title".into()),
            Value::String(draft.title.trim().to_string()),
        );
        fields.insert(
            self.config
                .mapping
                .get("status")
                .cloned()
                .unwrap_or_else(|| "status".into()),
            Value::String(
                draft
                    .status
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(&self.config.defaults.status)
                    .trim()
                    .to_string(),
            ),
        );
        fields.insert(
            "priority".into(),
            Value::String(
                draft
                    .priority
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(&self.config.defaults.priority)
                    .trim()
                    .to_string(),
            ),
        );
        if let Some(due) = draft.due.as_ref().filter(|value| !value.trim().is_empty()) {
            fields.insert("due".into(), Value::String(due.trim().to_string()));
        }
        if let Some(scheduled) = draft
            .scheduled
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            fields.insert(
                "scheduled".into(),
                Value::String(scheduled.trim().to_string()),
            );
        }
        if let Some(recurrence) = draft
            .recurrence
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            fields.insert(
                "recurrence".into(),
                Value::String(recurrence.trim().to_string()),
            );
            fields.insert(
                "recurrenceAnchor".into(),
                Value::String(
                    draft
                        .recurrence_anchor
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or("scheduled")
                        .trim()
                        .to_string(),
                ),
            );
        }
        if !draft.projects.is_empty() {
            fields.insert(
                self.config
                    .mapping
                    .get("projects")
                    .cloned()
                    .unwrap_or_else(|| "projects".into()),
                Value::Array(
                    draft
                        .projects
                        .iter()
                        .filter(|value| !value.trim().is_empty())
                        .map(|value| Value::String(value.trim().to_string()))
                        .collect(),
                ),
            );
        }
        fields.insert(
            self.config
                .mapping
                .get("date_created")
                .cloned()
                .unwrap_or_else(|| "dateCreated".into()),
            Value::String(now.clone()),
        );
        fields.insert(
            self.config
                .mapping
                .get("date_modified")
                .cloned()
                .unwrap_or_else(|| "dateModified".into()),
            Value::String(now.clone()),
        );
        let compat = collection
            .types()
            .get("task")
            .map(type_def_to_create_compat)
            .and_then(|task_type| {
                create_task_with_compat(&json!({
                    "taskType": task_type,
                    "frontmatter": Value::Object(fields.clone()),
                    "body": draft.details,
                    "fixedNow": now,
                }))
                .ok()
            });
        let path = compat
            .as_ref()
            .and_then(|result| result.get("path").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| {
                let slug = slugify(&draft.title);
                format!(
                    "{}/{}.md",
                    self.config.task_detection.default_folder.trim_matches('/'),
                    slug
                )
            });
        let create_fields = compat
            .as_ref()
            .and_then(|result| result.get("frontmatter").and_then(Value::as_object))
            .cloned()
            .unwrap_or(fields);
        let create_body = compat
            .as_ref()
            .and_then(|result| result.get("body").and_then(Value::as_str))
            .unwrap_or(&draft.details);
        let result = collection.create(&json!({
            "path": path,
            "type": "task",
            "fields": Value::Object(create_fields),
            "body": create_body,
        }));
        if let Some(error) = result.get("error") {
            return Err(anyhow!("create failed: {}", error));
        }
        self.read_task(&path)
    }

    pub fn canonical_project_link_for_task(&self, task: &TaskRecord) -> anyhow::Result<String> {
        let all_files = self.collection()?.build_all_files_data();
        let basename = Path::new(&task.path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string();
        anyhow::ensure!(
            !basename.is_empty(),
            "cannot derive canonical project link from empty basename"
        );
        let basename_matches = all_files
            .iter()
            .filter(|file| {
                Path::new(&file.path)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    == Some(basename.as_str())
            })
            .count();
        if basename_matches <= 1 {
            Ok(format!("[[{basename}]]"))
        } else {
            Ok(format!(
                "[[{}]]",
                task.path.strip_suffix(".md").unwrap_or(task.path.as_str())
            ))
        }
    }

    pub fn resolve_project_paths(&self, task: &TaskRecord) -> anyhow::Result<Vec<String>> {
        let all_files = self.collection()?.build_all_files_data();
        Ok(resolve_task_project_paths(task, &all_files))
    }

    pub fn update_title(&self, task: &TaskRecord, title: &str) -> anyhow::Result<TaskRecord> {
        let title = title.trim();
        anyhow::ensure!(!title.is_empty(), "title must not be empty");
        let collection = self.collection()?;
        let old_path = task.path.clone();
        let (new_path, renamed, normalized) = apply_title_update(
            &old_path,
            &task.normalized_frontmatter,
            title,
            &self.config.title.storage,
        );
        let fields = denormalize_frontmatter(&normalized, &self.field_mapping);
        let result = collection.update(&json!({ "path": old_path, "fields": fields }));
        if let Some(error) = result.get("error") {
            return Err(anyhow!("update failed: {}", error));
        }
        let mut current_path = task.path.clone();
        if renamed {
            let rename_result = collection.rename(&json!({
                "from": task.path,
                "to": new_path,
                "update_refs": true
            }));
            if let Some(error) = rename_result.get("error") {
                return Err(anyhow!("rename failed: {}", error));
            }
            current_path = new_path;
        }
        self.read_task(&current_path)
    }

    pub fn update_date_field(
        &self,
        task: &TaskRecord,
        field: &str,
        value: Option<&str>,
    ) -> anyhow::Result<TaskRecord> {
        let mut normalized = self.read_task(&task.path)?.normalized_frontmatter;
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => {
                normalized.insert(field.to_string(), Value::String(value.to_string()));
            }
            None => {
                normalized.remove(field);
            }
        }
        self.write_task_update(task, normalized)
    }

    pub fn update_scalar_field(
        &self,
        task: &TaskRecord,
        field: &str,
        value: Option<&str>,
    ) -> anyhow::Result<TaskRecord> {
        let mut normalized = self.read_task(&task.path)?.normalized_frontmatter;
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => {
                normalized.insert(field.to_string(), Value::String(value.to_string()));
            }
            None => {
                normalized.remove(field);
            }
        }
        self.write_task_update(task, normalized)
    }

    pub fn delete_task(&self, task: &TaskRecord) -> anyhow::Result<()> {
        let collection = self.collection()?;
        ensure_delete_allowed(false, true, &[])?;
        let result = collection.delete(&json!({
            "path": task.path,
            "check_backlinks": false
        }));
        if let Some(error) = result.get("error") {
            return Err(anyhow!("delete failed: {}", error));
        }
        Ok(())
    }

    pub fn toggle_archive(&self, task: &TaskRecord) -> anyhow::Result<TaskRecord> {
        if is_archived_task(task, &self.config.archive) {
            self.unarchive_task(task)
        } else {
            self.archive_task(task)
        }
    }

    fn archive_task(&self, task: &TaskRecord) -> anyhow::Result<TaskRecord> {
        let collection = self.collection()?;
        let mut normalized = self.read_task(&task.path)?.normalized_frontmatter;
        ensure_archive_markers(&mut normalized, &self.config.archive);
        normalized.insert(
            "dateModified".into(),
            Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        );
        let fields = denormalize_frontmatter(&normalized, &self.field_mapping);
        let result = collection.update(&json!({ "path": task.path, "fields": fields }));
        if let Some(error) = result.get("error") {
            return Err(anyhow!("update failed: {}", error));
        }

        let mut current_path = task.path.clone();
        if self.config.archive.move_on_archive {
            let target = archived_path_for_task(task, &self.config.archive);
            if target != task.path {
                let rename_result = collection.rename(&json!({
                    "from": task.path,
                    "to": target,
                    "update_refs": true
                }));
                if let Some(error) = rename_result.get("error") {
                    return Err(anyhow!("rename failed: {}", error));
                }
                current_path = target;
            }
        }
        self.read_task(&current_path)
    }

    fn unarchive_task(&self, task: &TaskRecord) -> anyhow::Result<TaskRecord> {
        let collection = self.collection()?;
        let mut current = self.read_task(&task.path)?;
        clear_archive_markers(&mut current.normalized_frontmatter, &self.config.archive);
        current.normalized_frontmatter.insert(
            "dateModified".into(),
            Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        );
        let fields = denormalize_frontmatter(&current.normalized_frontmatter, &self.field_mapping);
        let result = collection.update(&json!({ "path": current.path, "fields": fields }));
        if let Some(error) = result.get("error") {
            return Err(anyhow!("update failed: {}", error));
        }

        let mut current_path = current.path.clone();
        if self.config.archive.move_on_archive {
            let target = unarchived_path_for_task(&current, &self.config);
            if target != current.path {
                let rename_result = collection.rename(&json!({
                    "from": current.path,
                    "to": target,
                    "update_refs": true
                }));
                if let Some(error) = rename_result.get("error") {
                    return Err(anyhow!("rename failed: {}", error));
                }
                current_path = target;
            }
        }
        self.read_task(&current_path)
    }

    fn write_task_update(
        &self,
        task: &TaskRecord,
        mut normalized: Map<String, Value>,
    ) -> anyhow::Result<TaskRecord> {
        let collection = self.collection()?;
        normalized.insert(
            "dateModified".into(),
            Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        );
        let fields = denormalize_frontmatter(&normalized, &self.field_mapping);
        let result = collection.update(&json!({ "path": task.path, "fields": fields }));
        if let Some(error) = result.get("error") {
            return Err(anyhow!("update failed: {}", error));
        }
        self.read_task(&task.path)
    }
}

fn type_def_to_create_compat(type_def: &TypeDef) -> Value {
    let mut fields = Map::new();
    for (name, def) in &type_def.fields {
        let mut field = Map::new();
        if let Some(default) = &def.default {
            field.insert("default".into(), default.clone());
        }
        fields.insert(name.clone(), Value::Object(field));
    }

    let mut out = Map::new();
    if let Some(path_pattern) = &type_def.path_pattern {
        out.insert("path_pattern".into(), Value::String(path_pattern.clone()));
    }
    if let Some(match_rules) = &type_def.match_rules {
        let mut match_obj = Map::new();
        if let Some(where_clause) = &match_rules.where_clause {
            match_obj.insert("where".into(), where_clause.clone());
        }
        if !match_obj.is_empty() {
            out.insert("match".into(), Value::Object(match_obj));
        }
    }
    out.insert("fields".into(), Value::Object(fields));
    Value::Object(out)
}

fn slugify(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

pub fn is_archived_task(task: &TaskRecord, archive: &ArchiveConfig) -> bool {
    is_archived(&task.normalized_frontmatter, &task.path, archive)
}

fn archived_path_for_task(task: &TaskRecord, archive: &ArchiveConfig) -> String {
    let file_name = Path::new(&task.path)
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| slugify(&task.title) + ".md");
    format!("{}/{}", archive.folder.trim_matches('/'), file_name)
}

fn unarchived_path_for_task(task: &TaskRecord, config: &EffectiveConfig) -> String {
    let file_name = if config.title.storage == "filename" {
        format!("{}.md", slugify(&task.title))
    } else {
        Path::new(&task.path)
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("{}.md", slugify(&task.title)))
    };
    format!(
        "{}/{}",
        config.task_detection.default_folder.trim_matches('/'),
        file_name
    )
}

fn matches_filter(
    task: &TaskRecord,
    filter: TaskFilter,
    mapping: &FieldMapping,
    archive: &ArchiveConfig,
    reference_date: &str,
) -> bool {
    match filter {
        TaskFilter::All => true,
        TaskFilter::Open => {
            !is_completed_status(mapping, Some(&task.status)) && !is_archived_task(task, archive)
        }
        TaskFilter::Today => task
            .scheduled
            .as_deref()
            .map(get_date_part)
            .or_else(|| task.due.as_deref().map(get_date_part))
            .map(|value| value == get_date_part(reference_date))
            .unwrap_or(false),
        TaskFilter::Overdue => task
            .due
            .as_deref()
            .map(|due| {
                is_before_date_safe(due, reference_date)
                    && !is_completed_status(mapping, Some(&task.status))
            })
            .unwrap_or(false),
        TaskFilter::Tracked => task.has_active_time_entry,
    }
}

fn task_sort_key(a: &TaskRecord, b: &TaskRecord) -> Ordering {
    let left = a
        .scheduled
        .as_deref()
        .or(a.due.as_deref())
        .map(get_date_part)
        .unwrap_or_else(|| "9999-99-99".to_string());
    let right = b
        .scheduled
        .as_deref()
        .or(b.due.as_deref())
        .map(get_date_part)
        .unwrap_or_else(|| "9999-99-99".to_string());
    left.cmp(&right)
        .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
}

pub fn resolve_task_project_paths(
    task: &TaskRecord,
    all_files: &[ResolvedFileData],
) -> Vec<String> {
    project_links(task)
        .into_iter()
        .filter_map(|link| resolve_link_value(&link, &task.path, all_files))
        .fold(Vec::new(), |mut acc, path| {
            if !acc.contains(&path) {
                acc.push(path);
            }
            acc
        })
}

pub fn project_links(task: &TaskRecord) -> Vec<String> {
    task.normalized_frontmatter
        .get("projects")
        .map(read_link_values)
        .unwrap_or_default()
}

fn read_link_values(value: &Value) -> Vec<String> {
    match value {
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Value::String(value) => vec![value.to_string()],
        _ => Vec::new(),
    }
}

fn resolve_link_value(
    raw: &str,
    source_path: &str,
    all_files: &[ResolvedFileData],
) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let (target, format) = parse_link_target(raw)?;
    if target.is_empty() {
        return None;
    }

    if target.starts_with('/') {
        return resolve_path_target(target.trim_start_matches('/'), all_files);
    }

    if matches!(format, LinkFormat::Markdown | LinkFormat::Path)
        || target.starts_with("./")
        || target.starts_with("../")
    {
        let source_dir = Path::new(source_path)
            .parent()
            .and_then(|path| path.to_str())
            .unwrap_or("");
        let joined = if source_dir.is_empty() {
            target.to_string()
        } else {
            format!("{source_dir}/{target}")
        };
        return resolve_path_target(&normalize_path(&joined), all_files);
    }

    if target.contains('/') {
        return resolve_path_target(target, all_files);
    }

    let mut id_matches = Vec::new();
    let mut basename_matches = Vec::new();
    for file in all_files {
        if file
            .frontmatter
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id == target)
        {
            id_matches.push(file.path.clone());
        }
        if Path::new(&file.path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem == target)
        {
            basename_matches.push(file.path.clone());
        }
    }

    match id_matches.len() {
        1 => return id_matches.into_iter().next(),
        n if n > 1 => return None,
        _ => {}
    }
    match basename_matches.len() {
        1 => basename_matches.into_iter().next(),
        _ => None,
    }
}

fn resolve_path_target(target: &str, all_files: &[ResolvedFileData]) -> Option<String> {
    let target = normalize_path(target);
    let candidates = [target.clone(), format!("{target}.md")];
    candidates
        .into_iter()
        .find(|candidate| all_files.iter().any(|file| file.path == *candidate))
}

fn normalize_path(path: &str) -> String {
    let mut parts = Vec::new();
    let normalized = path.replace('\\', "/");
    for segment in normalized.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LinkFormat {
    Wikilink,
    Markdown,
    Path,
}

fn parse_link_target(raw: &str) -> Option<(&str, LinkFormat)> {
    if raw.starts_with("[[") && raw.ends_with("]]") {
        let inner = &raw[2..raw.len() - 2];
        let target = inner.split('|').next().unwrap_or(inner);
        let target = target.split('#').next().unwrap_or(target).trim();
        return Some((target, LinkFormat::Wikilink));
    }
    if raw.starts_with('[') && raw.ends_with(')') {
        let pivot = raw.rfind("](")?;
        let target = raw[pivot + 2..raw.len() - 1]
            .split('#')
            .next()
            .unwrap_or("")
            .trim();
        return Some((target, LinkFormat::Markdown));
    }
    Some((raw, LinkFormat::Path))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn write_collection(root: &Path) {
        fs::write(
            root.join("mdbase.yaml"),
            r#"spec_version: "0.2.1"
settings:
  types_folder: "_types"
  default_validation: "warn"
"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("_types")).unwrap();
        fs::write(
            root.join("tasknotes.yaml"),
            r#"task_detection:
  method: property
  property_name: status
  property_value: ""
"#,
        )
        .unwrap();
        fs::write(
            root.join("_types/task.md"),
            r#"---
name: task
path_pattern: "TaskNotes/Tasks/{title}.md"
fields:
  title:
    type: string
    required: true
  status:
    type: string
  priority:
    type: string
  due:
    type: date
  scheduled:
    type: date
  projects:
    type: list
  recurrence:
    type: string
  recurrenceAnchor:
    type: string
  completeInstances:
    type: list
  skippedInstances:
    type: list
  dateCreated:
    type: datetime
  dateModified:
    type: datetime
---
"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("TaskNotes/Tasks")).unwrap();
    }

    #[test]
    fn create_list_and_toggle_task() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());

        let repo = TaskRepository::open(tmp.path()).unwrap();
        repo.create_task("Test Task").unwrap();

        let tasks = repo.list_tasks(TaskFilter::Open, &today_local()).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Test Task");

        repo.toggle_complete(&tasks[0]).unwrap();
        let refreshed = repo.read_task(&tasks[0].path).unwrap();
        let today = today_local();
        assert_eq!(refreshed.status, "done");
        assert_eq!(
            refreshed
                .normalized_frontmatter
                .get("completedDate")
                .and_then(Value::as_str),
            Some(today.as_str())
        );
    }

    #[test]
    fn search_and_edit_task() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());

        let repo = TaskRepository::open(tmp.path()).unwrap();
        repo.create_task_from_draft(&TaskDraft {
            title: "Alpha task".into(),
            details: "original details".into(),
            due: Some("2026-04-01".into()),
            scheduled: None,
            priority: Some("high".into()),
            status: Some("open".into()),
            recurrence: None,
            recurrence_anchor: None,
            projects: vec![],
        })
        .unwrap();

        let tasks = repo
            .search_tasks(TaskFilter::All, Some("alpha"), &today_local())
            .unwrap();
        assert_eq!(tasks.len(), 1);
        repo.update_title(&tasks[0], "Renamed task").unwrap();
        let renamed = repo
            .search_tasks(TaskFilter::All, Some("renamed"), &today_local())
            .unwrap();
        assert_eq!(renamed.len(), 1);
        repo.update_date_field(&renamed[0], "scheduled", Some("2026-04-02"))
            .unwrap();

        let refreshed = repo.read_task(&renamed[0].path).unwrap();
        assert_eq!(refreshed.title, "Renamed task");
        assert_eq!(refreshed.body.trim_end(), "original details");
        assert_eq!(refreshed.scheduled.as_deref(), Some("2026-04-02"));
        assert_eq!(refreshed.priority.as_deref(), Some("high"));
    }

    #[test]
    fn project_links_resolve_from_wikilinks_and_markdown_links() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());

        let repo = TaskRepository::open(tmp.path()).unwrap();
        let project = repo
            .create_task_from_draft(&TaskDraft {
                title: "Project Alpha".into(),
                details: String::new(),
                due: None,
                scheduled: None,
                priority: None,
                status: Some("open".into()),
                recurrence: None,
                recurrence_anchor: None,
                projects: vec![],
            })
            .unwrap();

        let wikilink_task = repo
            .create_task_from_draft(&TaskDraft {
                title: "Child A".into(),
                details: String::new(),
                due: None,
                scheduled: None,
                priority: None,
                status: Some("open".into()),
                recurrence: None,
                recurrence_anchor: None,
                projects: vec!["[[Project Alpha]]".into()],
            })
            .unwrap();
        let markdown_task = repo
            .create_task_from_draft(&TaskDraft {
                title: "Child B".into(),
                details: String::new(),
                due: None,
                scheduled: None,
                priority: None,
                status: Some("open".into()),
                recurrence: None,
                recurrence_anchor: None,
                projects: vec!["[Project](./Project Alpha.md)".into()],
            })
            .unwrap();

        assert_eq!(
            repo.resolve_project_paths(&wikilink_task).unwrap(),
            vec![project.path.clone()]
        );
        assert_eq!(
            repo.resolve_project_paths(&markdown_task).unwrap(),
            vec![project.path.clone()]
        );
        assert_eq!(
            repo.canonical_project_link_for_task(&project).unwrap(),
            "[[Project Alpha]]"
        );
    }

    #[test]
    fn skip_recurring_task_today() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());
        let today = today_local();
        let repo = TaskRepository::open(tmp.path()).unwrap();
        repo.create_task("Recurring").unwrap();
        let task = repo
            .search_tasks(TaskFilter::All, Some("Recurring"), &today_local())
            .unwrap();
        assert_eq!(task.len(), 1);
        repo.update_date_field(&task[0], "scheduled", Some(today.as_str()))
            .unwrap();
        let mut recurring = repo.read_task(&task[0].path).unwrap();
        recurring
            .normalized_frontmatter
            .insert("recurrence".into(), Value::String("FREQ=DAILY".into()));
        recurring
            .normalized_frontmatter
            .insert("recurrenceAnchor".into(), Value::String("scheduled".into()));
        repo.write_task_update(&recurring, recurring.normalized_frontmatter.clone())
            .unwrap();
        let recurring = repo.read_task(&task[0].path).unwrap();
        repo.toggle_skip_today(&recurring).unwrap();
        let refreshed = repo.read_task(&recurring.path).unwrap();
        let skipped = refreshed
            .normalized_frontmatter
            .get("skippedInstances")
            .and_then(Value::as_array)
            .unwrap();
        assert!(skipped
            .iter()
            .filter_map(Value::as_str)
            .any(|value| value == today));
    }

    #[test]
    fn toggling_recurring_completion_twice_reopens_today_instance() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());
        let today = today_local();
        let repo = TaskRepository::open(tmp.path()).unwrap();
        repo.create_task("Recurring complete").unwrap();
        let task = repo
            .search_tasks(TaskFilter::All, Some("Recurring complete"), &today)
            .unwrap();
        assert_eq!(task.len(), 1);
        repo.update_date_field(&task[0], "scheduled", Some(today.as_str()))
            .unwrap();
        let mut recurring = repo.read_task(&task[0].path).unwrap();
        recurring
            .normalized_frontmatter
            .insert("recurrence".into(), Value::String("FREQ=DAILY".into()));
        recurring
            .normalized_frontmatter
            .insert("recurrenceAnchor".into(), Value::String("scheduled".into()));
        repo.write_task_update(&recurring, recurring.normalized_frontmatter.clone())
            .unwrap();

        let recurring = repo.read_task(&task[0].path).unwrap();
        repo.toggle_complete(&recurring).unwrap();
        let completed = repo.read_task(&recurring.path).unwrap();
        let completed_instances = completed
            .normalized_frontmatter
            .get("completeInstances")
            .and_then(Value::as_array)
            .unwrap();
        assert!(completed_instances
            .iter()
            .filter_map(Value::as_str)
            .any(|value| value == today));

        repo.toggle_complete(&completed).unwrap();
        let reopened = repo.read_task(&recurring.path).unwrap();
        let complete_instances = reopened
            .normalized_frontmatter
            .get("completeInstances")
            .and_then(Value::as_array)
            .unwrap();
        assert!(!complete_instances
            .iter()
            .filter_map(Value::as_str)
            .any(|value| value == today));
        assert_eq!(reopened.scheduled.as_deref(), Some(today.as_str()));
    }

    #[test]
    fn skip_recurring_task_today_toggles_off() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());
        let today = today_local();
        let repo = TaskRepository::open(tmp.path()).unwrap();
        repo.create_task("Recurring skip").unwrap();
        let task = repo
            .search_tasks(TaskFilter::All, Some("Recurring skip"), &today)
            .unwrap();
        assert_eq!(task.len(), 1);
        repo.update_date_field(&task[0], "scheduled", Some(today.as_str()))
            .unwrap();
        let mut recurring = repo.read_task(&task[0].path).unwrap();
        recurring
            .normalized_frontmatter
            .insert("recurrence".into(), Value::String("FREQ=DAILY".into()));
        recurring
            .normalized_frontmatter
            .insert("recurrenceAnchor".into(), Value::String("scheduled".into()));
        repo.write_task_update(&recurring, recurring.normalized_frontmatter.clone())
            .unwrap();

        let recurring = repo.read_task(&task[0].path).unwrap();
        repo.toggle_skip_today(&recurring).unwrap();
        let skipped = repo.read_task(&recurring.path).unwrap();
        assert!(skipped
            .normalized_frontmatter
            .get("skippedInstances")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .any(|value| value == today));

        repo.toggle_skip_today(&skipped).unwrap();
        let unskipped = repo.read_task(&recurring.path).unwrap();
        assert!(!unskipped
            .normalized_frontmatter
            .get("skippedInstances")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .any(|value| value == today));
    }

    #[test]
    fn create_with_recurrence_and_update_scalar_fields() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());

        let repo = TaskRepository::open(tmp.path()).unwrap();
        repo.create_task_from_draft(&TaskDraft {
            title: "Weekly review".into(),
            details: "check backlog".into(),
            due: Some("2026-04-03".into()),
            scheduled: Some("2026-04-01".into()),
            priority: Some("high".into()),
            status: Some("doing".into()),
            recurrence: Some("FREQ=WEEKLY".into()),
            recurrence_anchor: Some("completion".into()),
            projects: vec![],
        })
        .unwrap();

        let task = repo
            .search_tasks(TaskFilter::All, Some("weekly review"), &today_local())
            .unwrap();
        assert_eq!(task.len(), 1);
        assert_eq!(task[0].priority.as_deref(), Some("high"));
        assert_eq!(task[0].status, "doing");
        assert_eq!(
            task[0]
                .normalized_frontmatter
                .get("recurrence")
                .and_then(Value::as_str),
            Some("FREQ=WEEKLY")
        );
        assert_eq!(
            task[0]
                .normalized_frontmatter
                .get("recurrenceAnchor")
                .and_then(Value::as_str),
            Some("completion")
        );

        repo.update_scalar_field(&task[0], "priority", Some("low"))
            .unwrap();
        repo.update_scalar_field(&task[0], "status", Some("done"))
            .unwrap();
        let refreshed = repo.read_task(&task[0].path).unwrap();
        assert_eq!(refreshed.priority.as_deref(), Some("low"));
        assert_eq!(refreshed.status, "done");
    }

    #[test]
    fn today_filter_uses_reference_date() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());

        let repo = TaskRepository::open(tmp.path()).unwrap();
        repo.create_task_from_draft(&TaskDraft {
            title: "Today A".into(),
            details: String::new(),
            due: None,
            scheduled: Some("2026-04-10".into()),
            priority: None,
            status: Some("open".into()),
            recurrence: None,
            recurrence_anchor: None,
            projects: vec![],
        })
        .unwrap();
        repo.create_task_from_draft(&TaskDraft {
            title: "Today B".into(),
            details: String::new(),
            due: None,
            scheduled: Some("2026-04-11".into()),
            priority: None,
            status: Some("open".into()),
            recurrence: None,
            recurrence_anchor: None,
            projects: vec![],
        })
        .unwrap();

        let april_10 = repo.list_tasks(TaskFilter::Today, "2026-04-10").unwrap();
        let april_11 = repo.list_tasks(TaskFilter::Today, "2026-04-11").unwrap();

        assert_eq!(april_10.len(), 1);
        assert_eq!(april_10[0].title, "Today A");
        assert_eq!(april_11.len(), 1);
        assert_eq!(april_11[0].title, "Today B");
    }

    #[test]
    fn toggle_time_tracking_and_filter_tracked() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());

        let repo = TaskRepository::open(tmp.path()).unwrap();
        repo.create_task("Tracked Task").unwrap();
        let task = repo
            .search_tasks(TaskFilter::All, Some("Tracked Task"), &today_local())
            .unwrap();
        assert_eq!(task.len(), 1);
        assert!(!task[0].has_active_time_entry);

        repo.toggle_time_tracking(&task[0]).unwrap();
        let tracked = repo.read_task(&task[0].path).unwrap();
        assert!(tracked.has_active_time_entry);
        assert_eq!(tracked.time_entries.len(), 1);
        assert_eq!(
            repo.list_tasks(TaskFilter::Tracked, &today_local())
                .unwrap()
                .len(),
            1
        );

        repo.toggle_time_tracking(&tracked).unwrap();
        let stopped = repo.read_task(&task[0].path).unwrap();
        assert!(!stopped.has_active_time_entry);
        assert_eq!(
            repo.list_tasks(TaskFilter::Tracked, &today_local())
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn archived_tasks_are_hidden_from_open_and_can_move_folders() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());
        fs::write(
            tmp.path().join("tasknotes.yaml"),
            r#"task_detection:
  method: property
  property_name: status
  property_value: ""
archive:
  move_on_archive: true
  folder: "TaskNotes/Archive"
"#,
        )
        .unwrap();

        let repo = TaskRepository::open(tmp.path()).unwrap();
        repo.create_task("Archive me").unwrap();

        let open_tasks = repo.list_tasks(TaskFilter::Open, &today_local()).unwrap();
        assert_eq!(open_tasks.len(), 1);

        repo.toggle_archive(&open_tasks[0]).unwrap();

        let open_tasks = repo.list_tasks(TaskFilter::Open, &today_local()).unwrap();
        assert!(open_tasks.is_empty());

        let all_tasks = repo.list_tasks(TaskFilter::All, &today_local()).unwrap();
        assert_eq!(all_tasks.len(), 1);
        assert_eq!(all_tasks[0].status, "open");
        assert_eq!(
            all_tasks[0]
                .normalized_frontmatter
                .get("archived")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(all_tasks[0]
            .normalized_frontmatter
            .get("tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|tag| tag == "archived"));
        assert!(all_tasks[0].path.starts_with("TaskNotes/Archive/"));

        repo.toggle_archive(&all_tasks[0]).unwrap();
        let restored = repo.list_tasks(TaskFilter::Open, &today_local()).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].status, "open");
        assert_eq!(
            restored[0]
                .normalized_frontmatter
                .get("archived")
                .and_then(Value::as_bool),
            Some(false)
        );
        assert!(restored[0].path.starts_with("TaskNotes/Tasks/"));
    }

    #[test]
    fn archive_detection_and_writes_are_configurable() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());
        fs::write(
            tmp.path().join("tasknotes.yaml"),
            r#"task_detection:
  method: property
  property_name: status
  property_value: ""
archive:
  move_on_archive: false
  folder: "TaskNotes/Archive"
  tag: "cold-storage"
  field: "isArchived"
"#,
        )
        .unwrap();

        let repo = TaskRepository::open(tmp.path()).unwrap();
        repo.create_task("Custom archive").unwrap();

        let open_tasks = repo.list_tasks(TaskFilter::Open, &today_local()).unwrap();
        assert_eq!(open_tasks.len(), 1);
        repo.toggle_archive(&open_tasks[0]).unwrap();

        let all_tasks = repo.list_tasks(TaskFilter::All, &today_local()).unwrap();
        assert_eq!(all_tasks.len(), 1);
        assert_eq!(
            all_tasks[0]
                .normalized_frontmatter
                .get("isArchived")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(all_tasks[0]
            .normalized_frontmatter
            .get("tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|tag| tag == "cold-storage"));
        assert!(repo
            .list_tasks(TaskFilter::Open, &today_local())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn runtime_ignores_tasknotes_detection_for_membership() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());
        let repo = TaskRepository::open(tmp.path()).unwrap();
        repo.create_task("Typed task").unwrap();

        fs::create_dir_all(tmp.path().join("notes")).unwrap();
        fs::write(
            tmp.path().join("tasknotes.yaml"),
            r#"task_detection:
  method: property
  property_name: "definitely_not_present"
  property_value: "never"
"#,
        )
        .unwrap();

        let repo = TaskRepository::open(tmp.path()).unwrap();
        let titles: Vec<String> = repo
            .list_tasks(TaskFilter::All, &today_local())
            .unwrap()
            .into_iter()
            .map(|task| task.title)
            .collect();

        assert!(titles.iter().any(|title| title == "Typed task"));
    }

    #[test]
    fn delete_task_removes_it_from_lists() {
        let tmp = tempdir().unwrap();
        write_collection(tmp.path());

        let repo = TaskRepository::open(tmp.path()).unwrap();
        repo.create_task("Delete me").unwrap();
        let tasks = repo.list_tasks(TaskFilter::Open, &today_local()).unwrap();
        assert_eq!(tasks.len(), 1);

        repo.delete_task(&tasks[0]).unwrap();

        assert!(repo
            .list_tasks(TaskFilter::Open, &today_local())
            .unwrap()
            .is_empty());
        assert!(repo
            .list_tasks(TaskFilter::All, &today_local())
            .unwrap()
            .is_empty());
    }
}
