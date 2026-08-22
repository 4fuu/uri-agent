use super::wrapped_index;
use crate::catalog::{CatalogModel, ModelCatalog};
use crate::config::ActiveSettings;

#[derive(Clone)]
pub(super) struct ModelSelector {
    models: Vec<CatalogModel>,
    visible: Vec<usize>,
    query: String,
    selected: usize,
    current_provider: String,
    current_model: String,
}

impl ModelSelector {
    pub(super) async fn load(
        catalog: &ModelCatalog,
        active: &ActiveSettings,
        query: impl Into<String>,
    ) -> Self {
        let mut models = Vec::new();
        for provider in catalog.providers().await {
            models.extend(catalog.models(&provider).await);
        }
        models.sort_by(|left, right| {
            left.provider
                .cmp(&right.provider)
                .then_with(|| model_label(left).cmp(model_label(right)))
                .then_with(|| left.id.cmp(&right.id))
        });
        let mut selector = Self {
            models,
            visible: Vec::new(),
            query: query.into(),
            selected: 0,
            current_provider: active.provider.clone(),
            current_model: active.model.clone(),
        };
        selector.rebuild();
        selector.select_current();
        selector
    }

    #[cfg(test)]
    pub(super) fn from_models(
        models: Vec<CatalogModel>,
        current_provider: &str,
        current_model: &str,
    ) -> Self {
        let mut selector = Self {
            models,
            visible: Vec::new(),
            query: String::new(),
            selected: 0,
            current_provider: current_provider.to_string(),
            current_model: current_model.to_string(),
        };
        selector.rebuild();
        selector.select_current();
        selector
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn push(&mut self, character: char) {
        self.query.push(character);
        self.rebuild();
    }

    pub(super) fn paste(&mut self, text: &str) {
        self.query.push_str(text);
        self.rebuild();
    }

    pub(super) fn backspace(&mut self) {
        self.query.pop();
        self.rebuild();
    }

    pub(super) fn move_selection(&mut self, distance: isize) {
        self.selected = wrapped_index(self.selected, distance, self.visible.len());
    }

    pub(super) fn first(&mut self) {
        self.selected = 0;
    }

    pub(super) fn last(&mut self) {
        self.selected = self.visible.len().saturating_sub(1);
    }

    pub(super) fn select_position(&mut self, position: usize) {
        if position < self.visible.len() {
            self.selected = position;
        }
    }

    pub(super) fn selected_position(&self) -> usize {
        self.selected
    }

    pub(super) fn selected(&self) -> Option<&CatalogModel> {
        self.visible
            .get(self.selected)
            .and_then(|index| self.models.get(*index))
    }

    pub(super) fn visible(&self) -> impl Iterator<Item = &CatalogModel> {
        self.visible
            .iter()
            .filter_map(|index| self.models.get(*index))
    }

    pub(super) fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub(super) fn model_count(&self) -> usize {
        self.models.len()
    }

    pub(super) fn provider_count(&self) -> usize {
        self.models
            .iter()
            .map(|model| model.provider.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }

    pub(super) fn is_current(&self, model: &CatalogModel) -> bool {
        model.provider == self.current_provider && model.id == self.current_model
    }

    fn select_current(&mut self) {
        if self.query.is_empty()
            && let Some(position) = self.visible.iter().position(|index| {
                let model = &self.models[*index];
                self.is_current(model)
            })
        {
            self.selected = position;
        }
    }

    fn rebuild(&mut self) {
        let query = self.query.trim().to_ascii_lowercase();
        let mut scored = self
            .models
            .iter()
            .enumerate()
            .filter_map(|(index, model)| {
                let searchable = format!(
                    "{} {} {} {}",
                    model.provider, model.id, model.name, model.api
                )
                .to_ascii_lowercase();
                fuzzy_score(&searchable, &query).map(|score| (index, score))
            })
            .collect::<Vec<_>>();
        scored.sort_by(|(left_index, left_score), (right_index, right_score)| {
            left_score.cmp(right_score).then_with(|| {
                let left = &self.models[*left_index];
                let right = &self.models[*right_index];
                left.provider
                    .cmp(&right.provider)
                    .then_with(|| model_label(left).cmp(model_label(right)))
            })
        });
        self.visible = scored.into_iter().map(|(index, _)| index).collect();
        self.selected = 0;
    }
}

pub(super) fn model_label(model: &CatalogModel) -> &str {
    if model.name.trim().is_empty() {
        &model.id
    } else {
        &model.name
    }
}

pub(super) fn context_label(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

pub(super) fn reasoning(model: &CatalogModel) -> bool {
    model
        .metadata
        .get("reasoning")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn fuzzy_score(haystack: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    if haystack == query {
        return Some(0);
    }
    if haystack.starts_with(query) {
        return Some(1);
    }
    if let Some(position) = haystack.find(query) {
        return Some(position + 2);
    }
    let mut cursor = 0;
    let mut score = 100;
    for needle in query.chars() {
        let suffix = haystack.get(cursor..)?;
        let position = suffix.find(needle)?;
        score += position;
        cursor += position + needle.len_utf8();
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cmp::Ordering;
    use std::collections::BTreeMap;

    fn model(provider: &str, id: &str, name: &str) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            name: name.to_string(),
            api: "openai-responses".to_string(),
            provider: provider.to_string(),
            base_url: String::new(),
            headers: BTreeMap::new(),
            metadata: BTreeMap::from([
                ("contextWindow".to_string(), json!(200_000)),
                ("reasoning".to_string(), json!(true)),
            ]),
        }
    }

    #[test]
    fn searches_across_provider_id_and_display_name() {
        let mut selector = ModelSelector::from_models(
            vec![
                model("anthropic", "claude-sonnet", "Claude Sonnet"),
                model("openai", "gpt-5", "GPT 5"),
            ],
            "openai",
            "gpt-5",
        );
        selector.paste("sonnet");
        assert_eq!(selector.visible_len(), 1);
        assert_eq!(selector.selected().unwrap().provider, "anthropic");

        selector.backspace();
        selector.push('t');
        assert_eq!(selector.visible_len(), 1);
    }

    #[test]
    fn model_metadata_labels_are_compact() {
        let candidate = model("openai", "gpt-5", "GPT 5");
        assert_eq!(context_label(candidate.context_window()), "200k");
        assert!(reasoning(&candidate));
        assert_eq!(model_label(&candidate), "GPT 5");
    }

    #[test]
    fn moving_selection_wraps() {
        let mut selector = ModelSelector::from_models(
            vec![model("openai", "a", "A"), model("openai", "b", "B")],
            "openai",
            "a",
        );
        selector.move_selection(-1);
        assert_eq!(selector.selected_position(), 1);
        selector.move_selection(1);
        assert_eq!(selector.selected_position(), 0);
    }

    #[test]
    fn score_prefers_contiguous_matches() {
        assert_eq!(
            fuzzy_score("anthropic claude sonnet", "sonnet")
                .unwrap()
                .cmp(&fuzzy_score("some odd neural network", "sonnet").unwrap()),
            Ordering::Less
        );
    }
}
