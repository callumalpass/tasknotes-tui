use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use mdbase::expressions::ast::Expr;
use mdbase::expressions::evaluator::{evaluate, EvalContext, ResolvedFileData};
use mdbase::expressions::parser::Parser;
use mdbase::types::schema::TypeDef;
use mdbase::Collection;
use serde_json::Value;

use crate::app::ActiveProject;
use crate::config::ArchiveConfig;
use crate::date::{get_date_part, is_before_date_safe, today_local};
use crate::field_mapping::{is_completed_status, FieldMapping};
use crate::repository::{is_archived_task, resolve_task_project_paths, TaskRecord};
use crate::tui_config::{ViewConfig, ViewFilter};

#[derive(Clone)]
pub struct ViewEvalSupport {
    pub all_files: Arc<Vec<ResolvedFileData>>,
    pub backlinks_index: Arc<HashMap<String, Vec<String>>>,
    pub types: Arc<HashMap<String, TypeDef>>,
    pub type_names_by_path: Arc<HashMap<String, Vec<String>>>,
}

impl ViewEvalSupport {
    pub fn build(collection: &Collection) -> Self {
        let all_files = collection.build_all_files_data();
        let backlinks_index = collection.build_backlinks_index(&all_files);
        let type_names_by_path = all_files
            .iter()
            .map(|file| {
                (
                    file.path.clone(),
                    collection.determine_types_for_path(&file.frontmatter, Some(&file.path)),
                )
            })
            .collect();

        Self {
            all_files: Arc::new(all_files),
            backlinks_index: Arc::new(backlinks_index),
            types: Arc::new(collection.types().clone()),
            type_names_by_path: Arc::new(type_names_by_path),
        }
    }

    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            all_files: Arc::new(Vec::new()),
            backlinks_index: Arc::new(HashMap::new()),
            types: Arc::new(HashMap::new()),
            type_names_by_path: Arc::new(HashMap::new()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum CompiledViewFilter {
    BuiltIn(ViewFilter),
    Expression { source: String, expr: Expr },
    InvalidExpression { source: String, error: String },
}

impl CompiledViewFilter {
    pub fn from_filter(filter: &ViewFilter) -> Self {
        match filter {
            ViewFilter::Expression { value } => match Parser::parse(value) {
                Ok(expr) => Self::Expression {
                    source: value.clone(),
                    expr,
                },
                Err(error) => Self::InvalidExpression {
                    source: value.clone(),
                    error,
                },
            },
            other => Self::BuiltIn(other.clone()),
        }
    }

    pub fn matches(
        &self,
        task: &TaskRecord,
        focus_date: &str,
        mapping: &FieldMapping,
        archive: &ArchiveConfig,
        support: &ViewEvalSupport,
        active_project: Option<&ActiveProject>,
    ) -> bool {
        match self {
            Self::BuiltIn(filter) => {
                matches_builtin_filter(task, filter, mapping, archive, focus_date)
            }
            Self::Expression { expr, .. } => matches_expression_filter(
                task,
                expr,
                focus_date,
                mapping,
                archive,
                support,
                active_project,
            ),
            Self::InvalidExpression { .. } => false,
        }
    }

    pub fn error_message(&self) -> Option<String> {
        match self {
            Self::InvalidExpression { source, error } => {
                Some(format!("invalid expression `{source}`: {error}"))
            }
            _ => None,
        }
    }
}

pub fn compile_view_filters(views: &BTreeMap<u8, ViewConfig>) -> BTreeMap<u8, CompiledViewFilter> {
    views
        .iter()
        .map(|(slot, view)| (*slot, CompiledViewFilter::from_filter(&view.filter)))
        .collect()
}

fn matches_builtin_filter(
    task: &TaskRecord,
    filter: &ViewFilter,
    mapping: &FieldMapping,
    archive: &ArchiveConfig,
    focus_date: &str,
) -> bool {
    match filter {
        ViewFilter::All => !is_archived_task(task, archive),
        ViewFilter::Open => {
            !is_completed_status(mapping, Some(&task.status)) && !is_archived_task(task, archive)
        }
        ViewFilter::Date => task
            .scheduled
            .as_deref()
            .map(get_date_part)
            .or_else(|| task.due.as_deref().map(get_date_part))
            .map(|value| value == focus_date && !is_archived_task(task, archive))
            .unwrap_or(false),
        ViewFilter::Overdue => task
            .due
            .as_deref()
            .map(|due| {
                is_before_date_safe(due, focus_date)
                    && !is_completed_status(mapping, Some(&task.status))
                    && !is_archived_task(task, archive)
            })
            .unwrap_or(false),
        ViewFilter::Tracked => task.has_active_time_entry && !is_archived_task(task, archive),
        ViewFilter::Archived => is_archived_task(task, archive),
        ViewFilter::Status { value } => task.status == *value,
        ViewFilter::Expression { .. } => false,
    }
}

fn matches_expression_filter(
    task: &TaskRecord,
    expr: &Expr,
    focus_date: &str,
    mapping: &FieldMapping,
    archive: &ArchiveConfig,
    support: &ViewEvalSupport,
    active_project: Option<&ActiveProject>,
) -> bool {
    let context = build_eval_context(task, focus_date, mapping, archive, support, active_project);
    evaluate(expr, &context)
        .map(|value| is_truthy(&value))
        .unwrap_or(false)
}

fn build_eval_context(
    task: &TaskRecord,
    focus_date: &str,
    mapping: &FieldMapping,
    archive: &ArchiveConfig,
    support: &ViewEvalSupport,
    active_project: Option<&ActiveProject>,
) -> EvalContext {
    let mut frontmatter = task.normalized_frontmatter.clone();
    frontmatter.insert("focusDate".into(), Value::String(focus_date.to_string()));
    frontmatter.insert("today".into(), Value::String(today_local()));
    frontmatter.insert(
        "isCompleted".into(),
        Value::Bool(is_completed_status(mapping, Some(&task.status))),
    );
    frontmatter.insert("isTracked".into(), Value::Bool(task.has_active_time_entry));
    frontmatter.insert(
        "isArchived".into(),
        Value::Bool(is_archived_task(task, archive)),
    );
    frontmatter.insert("path".into(), Value::String(task.path.clone()));
    let project_paths = resolve_task_project_paths(task, &support.all_files);
    frontmatter.insert(
        "projectPaths".into(),
        Value::Array(project_paths.into_iter().map(Value::String).collect()),
    );
    frontmatter.insert(
        "hasActiveProject".into(),
        Value::Bool(active_project.is_some()),
    );
    frontmatter.insert(
        "activeProjectPath".into(),
        active_project
            .map(|project| Value::String(project.path.clone()))
            .unwrap_or(Value::Null),
    );
    frontmatter.insert(
        "activeProjectTitle".into(),
        active_project
            .map(|project| Value::String(project.title.clone()))
            .unwrap_or(Value::Null),
    );
    frontmatter.insert(
        "isActiveProject".into(),
        Value::Bool(
            active_project
                .map(|project| project.path == task.path)
                .unwrap_or(false),
        ),
    );

    EvalContext {
        frontmatter: Value::Object(frontmatter),
        raw_frontmatter: Some(Value::Object(task.raw_frontmatter.clone())),
        file_path: Some(task.path.clone()),
        body: Some(task.body.clone()),
        file_size: None,
        file_mtime: None,
        file_ctime: None,
        this_context: None,
        all_files: Some(support.all_files.clone()),
        traversal_depth: std::cell::Cell::new(0),
        backlinks_index: Some(support.backlinks_index.clone()),
        type_names: support.type_names_by_path.get(&task.path).cloned(),
        types: Some(support.types.clone()),
        note_namespace_source: Default::default(),
        string_concat: false,
    }
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EffectiveConfig;
    use serde_json::Map;

    use crate::field_mapping::default_field_mapping;

    fn task_record() -> TaskRecord {
        let mut normalized_frontmatter = Map::new();
        normalized_frontmatter.insert("status".into(), Value::String("doing".into()));
        normalized_frontmatter.insert("priority".into(), Value::String("high".into()));
        normalized_frontmatter.insert("scheduled".into(), Value::String("2026-04-10".into()));
        normalized_frontmatter.insert(
            "timeEntries".into(),
            Value::Array(vec![serde_json::json!({"start": "2026-04-10T09:00:00Z"})]),
        );

        TaskRecord {
            path: "TaskNotes/Tasks/example.md".into(),
            title: "Example".into(),
            status: "doing".into(),
            priority: Some("high".into()),
            due: None,
            scheduled: Some("2026-04-10".into()),
            time_entries: vec![serde_json::json!({"start": "2026-04-10T09:00:00Z"})],
            has_active_time_entry: true,
            body: "Ship the release".into(),
            normalized_frontmatter,
            raw_frontmatter: Map::new(),
        }
    }

    #[test]
    fn expression_view_matches_task_fields_and_helpers() {
        let compiled = CompiledViewFilter::from_filter(&ViewFilter::Expression {
            value:
                "status == \"doing\" && priority == \"high\" && scheduled == focusDate && isTracked"
                    .into(),
        });

        assert!(compiled.matches(
            &task_record(),
            "2026-04-10",
            &default_field_mapping(),
            &EffectiveConfig::default().archive,
            &ViewEvalSupport::empty(),
            None,
        ));
    }

    #[test]
    fn expression_view_can_access_file_body() {
        let compiled = CompiledViewFilter::from_filter(&ViewFilter::Expression {
            value: "file.body.contains(\"release\")".into(),
        });

        assert!(compiled.matches(
            &task_record(),
            "2026-04-10",
            &default_field_mapping(),
            &EffectiveConfig::default().archive,
            &ViewEvalSupport::empty(),
            None,
        ));
    }

    #[test]
    fn invalid_expression_is_reported() {
        let compiled = CompiledViewFilter::from_filter(&ViewFilter::Expression {
            value: "status ==".into(),
        });

        assert!(compiled.error_message().is_some());
        assert!(!compiled.matches(
            &task_record(),
            "2026-04-10",
            &default_field_mapping(),
            &EffectiveConfig::default().archive,
            &ViewEvalSupport::empty(),
            None,
        ));
    }
}
