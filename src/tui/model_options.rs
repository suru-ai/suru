//! Typed Model Option staging and choice-picker state.

use crate::protocol::{
    AgentSelection, ModelAvailability, ModelDescriptor, ModelOptionChoiceId, ModelOptionDescriptor,
    ModelOptionId, ModelOptionKind, ModelOptionRole, ModelOptionSelection, ModelOptionValue,
};

use super::model_picker::ModelPickerPurpose;

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChoiceValue {
    ProviderDefault,
    Select(ModelOptionChoiceId),
    Toggle(bool),
}

#[derive(Clone, Debug)]
struct ChoicePicker {
    option: ModelOptionId,
    selected: ChoiceValue,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ModelOptions {
    purpose: ModelPickerPurpose,
    model: Option<ModelDescriptor>,
    staged: Option<AgentSelection>,
    selected: usize,
    choices: Option<ChoicePicker>,
}

#[derive(Clone, Debug)]
pub(super) struct ModelOptionRow {
    pub(super) label: String,
    pub(super) description: Option<String>,
    pub(super) value: String,
    pub(super) selected: bool,
    pub(super) available: bool,
}

#[derive(Clone, Debug)]
pub(super) struct ModelOptionChoiceRow {
    pub(super) label: String,
    pub(super) description: Option<String>,
    pub(super) selected: bool,
    pub(super) current: bool,
    pub(super) available: bool,
}

impl ModelOptions {
    pub(super) fn open(&mut self, model: ModelDescriptor, current: Option<&AgentSelection>) {
        self.open_for(model, current, ModelPickerPurpose::AgentSelection);
    }

    pub(super) fn open_for(
        &mut self,
        model: ModelDescriptor,
        current: Option<&AgentSelection>,
        purpose: ModelPickerPurpose,
    ) {
        let staged = stage_selection(&model, current, purpose);
        self.purpose = purpose;
        self.model = Some(model);
        self.staged = Some(staged);
        self.selected = 0;
        self.choices = None;
    }

    pub(super) fn purpose(&self) -> ModelPickerPurpose {
        self.purpose
    }

    pub(super) fn close(&mut self) {
        self.purpose = ModelPickerPurpose::default();
        self.model = None;
        self.staged = None;
        self.selected = 0;
        self.choices = None;
    }

    pub(super) fn refresh(&mut self, model: ModelDescriptor) {
        let Some(staged) = self.staged.as_ref() else {
            return;
        };
        if staged.provider != model.provider || staged.model != model.id {
            return;
        }
        self.staged = Some(stage_selection(&model, Some(staged), self.purpose));
        self.selected = if self.is_confirm_selected() {
            model.options.len()
        } else {
            self.selected.min(model.options.len().saturating_sub(1))
        };
        if self.choices.as_ref().is_some_and(|picker| {
            !model
                .options
                .iter()
                .any(|descriptor| descriptor.id == picker.option)
        }) {
            self.choices = None;
        }
        self.model = Some(model);
    }

    pub(super) fn mark_model_unavailable(&mut self) {
        if let Some(model) = &mut self.model {
            model.availability = ModelAvailability::Unavailable;
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.model.is_some()
    }

    pub(super) fn is_choice_picker_open(&self) -> bool {
        self.choices.is_some()
    }

    pub(super) fn is_confirm_selected(&self) -> bool {
        self.choices.is_none()
            && self
                .model
                .as_ref()
                .is_some_and(|model| self.selected == model.options.len())
    }

    pub(super) fn model(&self) -> Option<&ModelDescriptor> {
        self.model.as_ref()
    }

    pub(super) fn select_previous(&mut self) {
        if self.choices.is_some() {
            self.move_choice(-1);
        } else {
            self.move_option(-1);
        }
    }

    pub(super) fn select_next(&mut self) {
        if self.choices.is_some() {
            self.move_choice(1);
        } else {
            self.move_option(1);
        }
    }

    pub(super) fn choose(&mut self) {
        if self.choices.is_some() {
            self.choose_value();
        } else {
            self.open_choices();
        }
    }

    pub(super) fn apply(&self) -> Option<AgentSelection> {
        self.is_valid().then(|| {
            self.staged
                .clone()
                .expect("an open options screen has staged selection")
        })
    }

    pub(super) fn is_valid(&self) -> bool {
        let (Some(model), Some(staged)) = (&self.model, &self.staged) else {
            return false;
        };
        let effective = selection_preserving_current_values(model, staged);
        model.availability == ModelAvailability::Available
            && model.materialize_agent_selection(Some(&effective)).is_ok()
    }

    pub(super) fn rows(&self) -> Vec<ModelOptionRow> {
        let (Some(model), Some(staged)) = (&self.model, &self.staged) else {
            return Vec::new();
        };
        model
            .options
            .iter()
            .enumerate()
            .map(|(index, descriptor)| {
                let value = selected_value(staged, &descriptor.id);
                ModelOptionRow {
                    label: descriptor.label.clone(),
                    description: descriptor.description.clone(),
                    value: value
                        .map(|value| option_value_label(descriptor, value))
                        .unwrap_or_else(|| "Provider default".to_owned()),
                    selected: self.choices.is_none() && index == self.selected,
                    available: value
                        .is_none_or(|value| option_value_is_available(descriptor, value)),
                }
            })
            .collect()
    }

    pub(super) fn choice_rows(&self) -> Vec<ModelOptionChoiceRow> {
        let (Some(model), Some(staged), Some(picker)) = (&self.model, &self.staged, &self.choices)
        else {
            return Vec::new();
        };
        let Some(descriptor) = model
            .options
            .iter()
            .find(|descriptor| descriptor.id == picker.option)
        else {
            return Vec::new();
        };
        let current = selected_value(staged, &descriptor.id);
        choice_values(descriptor, current, self.purpose)
            .into_iter()
            .map(|candidate| {
                let (label, description, available) = choice_details(descriptor, &candidate);
                ModelOptionChoiceRow {
                    label,
                    description,
                    selected: picker.selected == candidate,
                    current: current.map_or(candidate == ChoiceValue::ProviderDefault, |value| {
                        choice_matches_value(&candidate, value)
                    }),
                    available,
                }
            })
            .collect()
    }

    pub(super) fn selected_descriptor(&self) -> Option<&ModelOptionDescriptor> {
        let model = self.model.as_ref()?;
        let index = if let Some(picker) = &self.choices {
            model
                .options
                .iter()
                .position(|descriptor| descriptor.id == picker.option)?
        } else {
            self.selected
        };
        model.options.get(index)
    }

    fn move_option(&mut self, distance: isize) {
        let count = self
            .model
            .as_ref()
            .map_or(0, |model| model.options.len() + 1);
        if count > 0 {
            self.selected = (self.selected as isize + distance).rem_euclid(count as isize) as usize;
        }
    }

    fn open_choices(&mut self) {
        let (Some(model), Some(staged)) = (&self.model, &self.staged) else {
            return;
        };
        let Some(descriptor) = model.options.get(self.selected) else {
            return;
        };
        let value = selected_value(staged, &descriptor.id);
        self.choices = Some(ChoicePicker {
            option: descriptor.id.clone(),
            selected: value.map_or(ChoiceValue::ProviderDefault, ChoiceValue::from),
        });
    }

    fn move_choice(&mut self, distance: isize) {
        let (Some(model), Some(staged), Some(picker)) =
            (&self.model, &self.staged, &mut self.choices)
        else {
            return;
        };
        let Some(descriptor) = model
            .options
            .iter()
            .find(|descriptor| descriptor.id == picker.option)
        else {
            return;
        };
        let values = choice_values(
            descriptor,
            selected_value(staged, &descriptor.id),
            self.purpose,
        );
        if values.is_empty() {
            return;
        }
        let current = values
            .iter()
            .position(|candidate| candidate == &picker.selected)
            .unwrap_or(0);
        let next = (current as isize + distance).rem_euclid(values.len() as isize) as usize;
        picker.selected = values[next].clone();
    }

    fn choose_value(&mut self) {
        let (Some(model), Some(staged), Some(picker)) =
            (&self.model, &mut self.staged, &self.choices)
        else {
            return;
        };
        let Some(descriptor) = model
            .options
            .iter()
            .find(|descriptor| descriptor.id == picker.option)
        else {
            return;
        };
        let (_, _, available) = choice_details(descriptor, &picker.selected);
        if !available {
            return;
        }
        if let Some(value) = picker.selected.clone().into_value() {
            if let Some(selection) = staged.options.iter_mut().find(|s| s.id == descriptor.id) {
                selection.value = value;
            } else {
                staged.options.push(ModelOptionSelection {
                    id: descriptor.id.clone(),
                    value,
                });
            }
        } else {
            staged
                .options
                .retain(|selection| selection.id != descriptor.id);
        }
        self.choices = None;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ReasoningCycle {
    Advanced(AgentSelection),
    Unavailable(String),
}

/// Advances the unique Reasoning Effort option to the next Provider-advertised
/// available choice, wrapping past the end. The explicit default participates
/// as an ordinary choice.
pub(super) fn cycle_reasoning_effort(
    model: &ModelDescriptor,
    current: Option<&AgentSelection>,
) -> ReasoningCycle {
    let mut reasoning = model
        .options
        .iter()
        .filter(|descriptor| descriptor.role == ModelOptionRole::ReasoningEffort);
    let Some(descriptor) = reasoning.next() else {
        return ReasoningCycle::Unavailable(format!(
            "{} has no Reasoning Effort option",
            model.display_name
        ));
    };
    if reasoning.next().is_some() {
        return ReasoningCycle::Unavailable(format!(
            "{} advertises more than one Reasoning Effort option",
            model.display_name
        ));
    }
    let ModelOptionKind::Select { choices, .. } = &descriptor.kind else {
        return ReasoningCycle::Unavailable(format!(
            "{} has no Reasoning Effort choices",
            model.display_name
        ));
    };
    let mut selection = if current.is_some_and(|selection| {
        selection.provider == model.provider && selection.model == model.id
    }) {
        selection_preserving_current_values(model, current.expect("same Model was checked"))
    } else {
        model.default_agent_selection()
    };
    let current_choice = match selected_value(&selection, &descriptor.id) {
        Some(ModelOptionValue::Select { choice }) => Some(choice.clone()),
        _ => None,
    };
    let after_current = current_choice
        .as_ref()
        .and_then(|current| choices.iter().position(|choice| &choice.id == current))
        .map_or(0, |index| index + 1);
    let next = (0..choices.len())
        .map(|offset| &choices[(after_current + offset) % choices.len()])
        .find(|choice| choice.availability == ModelAvailability::Available);
    let Some(next) = next.filter(|next| Some(&next.id) != current_choice.as_ref()) else {
        return ReasoningCycle::Unavailable(format!(
            "{} has no alternate Reasoning Effort choice",
            model.display_name
        ));
    };
    let choice = next.id.clone();
    let value = selection
        .options
        .iter_mut()
        .find(|option| option.id == descriptor.id)
        .expect("a complete Agent Selection covers every advertised option");
    value.value = ModelOptionValue::Select { choice };
    ReasoningCycle::Advanced(selection)
}

impl From<&ModelOptionValue> for ChoiceValue {
    fn from(value: &ModelOptionValue) -> Self {
        match value {
            ModelOptionValue::Select { choice } => Self::Select(choice.clone()),
            ModelOptionValue::Toggle { enabled } => Self::Toggle(*enabled),
        }
    }
}

impl ChoiceValue {
    fn into_value(self) -> Option<ModelOptionValue> {
        match self {
            Self::ProviderDefault => None,
            Self::Select(choice) => Some(ModelOptionValue::Select { choice }),
            Self::Toggle(enabled) => Some(ModelOptionValue::Toggle { enabled }),
        }
    }
}

/// Settings retain only explicit overrides; ordinary Agent Selections remain complete.
fn stage_selection(
    model: &ModelDescriptor,
    current: Option<&AgentSelection>,
    purpose: ModelPickerPurpose,
) -> AgentSelection {
    let current = current
        .filter(|selection| selection.provider == model.provider && selection.model == model.id);
    let mut staged = current.map_or_else(
        || model.default_agent_selection(),
        |current| selection_preserving_current_values(model, current),
    );
    if matches!(purpose, ModelPickerPurpose::Setting(_)) {
        staged.options.retain(|option| {
            current.is_some_and(|current| current.options.iter().any(|old| old.id == option.id))
        });
    }
    staged
}

fn selection_preserving_current_values(
    model: &ModelDescriptor,
    current: &AgentSelection,
) -> AgentSelection {
    let defaults = model.default_agent_selection();
    AgentSelection {
        provider: model.provider.clone(),
        model: model.id.clone(),
        options: model
            .options
            .iter()
            .zip(defaults.options)
            .map(|(descriptor, default)| {
                current
                    .options
                    .iter()
                    .find(|selection| selection.id == descriptor.id)
                    .cloned()
                    .unwrap_or(default)
            })
            .collect(),
    }
}

fn selected_value<'a>(
    selection: &'a AgentSelection,
    option: &ModelOptionId,
) -> Option<&'a ModelOptionValue> {
    selection
        .options
        .iter()
        .find(|selection| &selection.id == option)
        .map(|selection| &selection.value)
}

fn option_value_label(descriptor: &ModelOptionDescriptor, value: &ModelOptionValue) -> String {
    match (&descriptor.kind, value) {
        (ModelOptionKind::Select { choices, .. }, ModelOptionValue::Select { choice }) => choices
            .iter()
            .find(|candidate| candidate.id == *choice)
            .map_or_else(
                || format!("{choice} (unavailable)"),
                |choice| choice.label.clone(),
            ),
        (ModelOptionKind::Toggle { .. }, ModelOptionValue::Toggle { enabled }) => {
            if *enabled { "On" } else { "Off" }.to_owned()
        }
        _ => "Unavailable".to_owned(),
    }
}

fn option_value_is_available(descriptor: &ModelOptionDescriptor, value: &ModelOptionValue) -> bool {
    match (&descriptor.kind, value) {
        (ModelOptionKind::Select { choices, .. }, ModelOptionValue::Select { choice }) => {
            choices.iter().any(|candidate| {
                candidate.id == *choice && candidate.availability == ModelAvailability::Available
            })
        }
        (ModelOptionKind::Toggle { .. }, ModelOptionValue::Toggle { .. }) => true,
        _ => false,
    }
}

fn choice_values(
    descriptor: &ModelOptionDescriptor,
    current: Option<&ModelOptionValue>,
    purpose: ModelPickerPurpose,
) -> Vec<ChoiceValue> {
    let mut values = match &descriptor.kind {
        ModelOptionKind::Select { choices, .. } => {
            let mut values = choices
                .iter()
                .map(|choice| ChoiceValue::Select(choice.id.clone()))
                .collect::<Vec<_>>();
            if let Some(ModelOptionValue::Select { choice }) = current
                && !choices.iter().any(|candidate| candidate.id == *choice)
            {
                values.push(ChoiceValue::Select(choice.clone()));
            }
            values
        }
        ModelOptionKind::Toggle { .. } => {
            vec![ChoiceValue::Toggle(false), ChoiceValue::Toggle(true)]
        }
    };
    if matches!(purpose, ModelPickerPurpose::Setting(_)) {
        values.insert(0, ChoiceValue::ProviderDefault);
    }
    values
}

fn choice_details(
    descriptor: &ModelOptionDescriptor,
    value: &ChoiceValue,
) -> (String, Option<String>, bool) {
    match (&descriptor.kind, value) {
        (_, ChoiceValue::ProviderDefault) => (
            "Provider default".to_owned(),
            Some("Follow this Model's current default without pinning a value".to_owned()),
            true,
        ),
        (ModelOptionKind::Select { choices, .. }, ChoiceValue::Select(selected)) => choices
            .iter()
            .find(|choice| choice.id == *selected)
            .map_or_else(
                || (selected.to_string(), None, false),
                |choice| {
                    (
                        choice.label.clone(),
                        choice.description.clone(),
                        choice.availability == ModelAvailability::Available,
                    )
                },
            ),
        (ModelOptionKind::Toggle { .. }, ChoiceValue::Toggle(enabled)) => {
            (if *enabled { "On" } else { "Off" }.to_owned(), None, true)
        }
        _ => ("Unavailable".to_owned(), None, false),
    }
}

fn choice_matches_value(choice: &ChoiceValue, value: &ModelOptionValue) -> bool {
    match (choice, value) {
        (ChoiceValue::Select(left), ModelOptionValue::Select { choice: right }) => left == right,
        (ChoiceValue::Toggle(left), ModelOptionValue::Toggle { enabled: right }) => left == right,
        _ => false,
    }
}
