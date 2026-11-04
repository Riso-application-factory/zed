use collections::HashSet;
use gpui::SharedString;

use crate::ModelEntry;

pub const MAX_OPTIONS: usize = 4096;
pub const MAX_GROUPS: usize = 4096;
pub const MAX_RAW_VALUES: usize = 4096;
pub const MAX_TOTAL_METADATA_BYTES: usize = 1024 * 1024;
pub const MAX_MODEL_ID_BYTES: usize = 512;
pub const MAX_NAME_BYTES: usize = 1024;
pub const MAX_DESCRIPTION_BYTES: usize = 4096;

/// A configuration option as an agent offered it, translated out of the protocol's types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferedOption {
    pub is_model_category: bool,
    /// `None` when the option is not a single-choice selection.
    pub select: Option<OfferedSelect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OfferedSelect {
    Ungrouped(Vec<OfferedValue>),
    Grouped(Vec<Vec<OfferedValue>>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferedValue {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtractError {
    NoModelOption,
    CountLimit,
    SizeLimit,
    InvalidCharacters,
}

/// Sizes of the raw protocol data, checked by the adapter before it copies anything.
#[derive(Clone, Copy, Debug, Default)]
pub struct RawBudget {
    pub options: usize,
    pub groups: usize,
    pub values: usize,
    pub metadata_bytes: usize,
}

pub fn check_raw_budget(budget: RawBudget) -> Result<(), ExtractError> {
    if budget.options > MAX_OPTIONS || budget.groups > MAX_GROUPS || budget.values > MAX_RAW_VALUES
    {
        return Err(ExtractError::CountLimit);
    }
    if budget.metadata_bytes > MAX_TOTAL_METADATA_BYTES {
        return Err(ExtractError::SizeLimit);
    }
    Ok(())
}

impl OfferedSelect {
    fn values(&self) -> Box<dyn Iterator<Item = &OfferedValue> + '_> {
        match self {
            OfferedSelect::Ungrouped(values) => Box::new(values.iter()),
            OfferedSelect::Grouped(groups) => Box::new(groups.iter().flatten()),
        }
    }

    fn group_count(&self) -> usize {
        match self {
            OfferedSelect::Ungrouped(_) => 0,
            OfferedSelect::Grouped(groups) => groups.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.values().next().is_none()
    }
}

/// Returns the ordered, deduplicated models of the first usable model selection.
///
/// A selection that breaks a limit rejects the whole list instead of falling back to another
/// option, so a partially valid list is never mistaken for the agent's full offer.
pub fn extract_models(options: &[OfferedOption]) -> Result<Vec<ModelEntry>, ExtractError> {
    if options.len() > MAX_OPTIONS {
        return Err(ExtractError::CountLimit);
    }
    let select = options
        .iter()
        .filter(|option| option.is_model_category)
        .filter_map(|option| option.select.as_ref())
        .find(|select| !select.is_empty())
        .ok_or(ExtractError::NoModelOption)?;

    if select.group_count() > MAX_GROUPS || select.values().count() > MAX_RAW_VALUES {
        return Err(ExtractError::CountLimit);
    }

    let mut total_bytes = 0;
    let mut seen_ids = HashSet::default();
    let mut models = Vec::new();
    for value in select.values() {
        validate_field(&value.value, 1, MAX_MODEL_ID_BYTES)?;
        validate_field(&value.name, 0, MAX_NAME_BYTES)?;
        if let Some(description) = &value.description {
            validate_field(description, 0, MAX_DESCRIPTION_BYTES)?;
        }
        total_bytes += value.value.len()
            + value.name.len()
            + value.description.as_ref().map_or(0, String::len);
        if total_bytes > MAX_TOTAL_METADATA_BYTES {
            return Err(ExtractError::SizeLimit);
        }
        if seen_ids.insert(value.value.as_str()) {
            models.push(ModelEntry {
                model_id: SharedString::from(value.value.clone()),
                name: SharedString::from(value.name.clone()),
                description: value.description.clone().map(SharedString::from),
            });
        }
    }

    if models.is_empty() {
        return Err(ExtractError::NoModelOption);
    }
    Ok(models)
}

fn validate_field(field: &str, min_bytes: usize, max_bytes: usize) -> Result<(), ExtractError> {
    if field.len() < min_bytes || field.len() > max_bytes {
        return Err(ExtractError::SizeLimit);
    }
    if field.chars().any(is_forbidden_character) {
        return Err(ExtractError::InvalidCharacters);
    }
    Ok(())
}

// Model fields end up in logs and in prompts for coordinating agents, so control and
// bidirectional formatting characters could forge lines or reorder displayed text.
fn is_forbidden_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(id: &str) -> OfferedValue {
        OfferedValue {
            value: id.to_string(),
            name: format!("{id} name"),
            description: None,
        }
    }

    fn model_option(select: OfferedSelect) -> OfferedOption {
        OfferedOption {
            is_model_category: true,
            select: Some(select),
        }
    }

    fn ids(models: &[ModelEntry]) -> Vec<&str> {
        models.iter().map(|model| model.model_id.as_ref()).collect()
    }

    #[test]
    fn flattens_groups_in_order_and_keeps_first_duplicate() {
        let mut duplicate = value("opus");
        duplicate.name = "Second opus".to_string();
        let options = [model_option(OfferedSelect::Grouped(vec![
            vec![value("default"), value("opus")],
            vec![duplicate, value("sonnet")],
        ]))];

        let models = extract_models(&options).unwrap();

        assert_eq!(ids(&models), ["default", "opus", "sonnet"]);
        assert_eq!(models[1].name.as_ref(), "opus name");
    }

    #[test]
    fn skips_options_that_are_not_usable_model_selections() {
        let options = [
            OfferedOption {
                is_model_category: false,
                select: Some(OfferedSelect::Ungrouped(vec![value("plan")])),
            },
            OfferedOption {
                is_model_category: true,
                select: None,
            },
            model_option(OfferedSelect::Ungrouped(Vec::new())),
            model_option(OfferedSelect::Ungrouped(vec![value("gpt-5")])),
            model_option(OfferedSelect::Ungrouped(vec![value("ignored")])),
        ];

        assert_eq!(ids(&extract_models(&options).unwrap()), ["gpt-5"]);
    }

    #[test]
    fn reports_missing_model_option() {
        assert_eq!(extract_models(&[]), Err(ExtractError::NoModelOption));
        let only_empty_groups = [model_option(OfferedSelect::Grouped(vec![Vec::new()]))];
        assert_eq!(
            extract_models(&only_empty_groups),
            Err(ExtractError::NoModelOption)
        );
    }

    #[test]
    fn rejects_whole_list_on_field_limits() {
        let too_long_id = "a".repeat(MAX_MODEL_ID_BYTES + 1);
        let longest_id = "a".repeat(MAX_MODEL_ID_BYTES);
        let mut long_description = value("described");
        long_description.description = Some("d".repeat(MAX_DESCRIPTION_BYTES + 1));

        for (values, expected) in [
            (vec![value("ok"), value("")], Err(ExtractError::SizeLimit)),
            (vec![value("ok"), value(&too_long_id)], Err(ExtractError::SizeLimit)),
            (vec![value("ok"), long_description], Err(ExtractError::SizeLimit)),
            (vec![value("ok"), value("bad\nid")], Err(ExtractError::InvalidCharacters)),
            (vec![value("bad\u{202E}id")], Err(ExtractError::InvalidCharacters)),
        ] {
            assert_eq!(
                extract_models(&[model_option(OfferedSelect::Ungrouped(values))]),
                expected
            );
        }
        assert!(
            extract_models(&[model_option(OfferedSelect::Ungrouped(vec![value(
                &longest_id
            )]))])
            .is_ok()
        );
    }

    #[test]
    fn counts_duplicates_against_value_limit() {
        let values = vec![value("same"); MAX_RAW_VALUES + 1];
        assert_eq!(
            extract_models(&[model_option(OfferedSelect::Ungrouped(values))]),
            Err(ExtractError::CountLimit)
        );
    }

    #[test]
    fn rejects_total_metadata_over_budget() {
        let values = (0..300)
            .map(|index| OfferedValue {
                value: format!("model-{index}"),
                name: String::new(),
                description: Some("d".repeat(MAX_DESCRIPTION_BYTES)),
            })
            .collect();
        assert_eq!(
            extract_models(&[model_option(OfferedSelect::Ungrouped(values))]),
            Err(ExtractError::SizeLimit)
        );
    }

    #[test]
    fn raw_budget_rejects_counts_and_bytes() {
        assert_eq!(check_raw_budget(RawBudget::default()), Ok(()));
        assert_eq!(
            check_raw_budget(RawBudget {
                options: MAX_OPTIONS + 1,
                ..RawBudget::default()
            }),
            Err(ExtractError::CountLimit)
        );
        assert_eq!(
            check_raw_budget(RawBudget {
                metadata_bytes: MAX_TOTAL_METADATA_BYTES + 1,
                ..RawBudget::default()
            }),
            Err(ExtractError::SizeLimit)
        );
    }
}
