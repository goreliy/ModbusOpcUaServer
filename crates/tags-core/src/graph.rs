//! Validated static DAG. Kahn's algorithm is iterative even for deep chains.
use mb_types::TagId;
use std::collections::{HashMap, VecDeque};

pub(crate) struct DependencyGraph {
    pub order: Vec<TagId>,
    pub dependencies: Vec<Vec<TagId>>,
    pub dependents: Vec<Vec<TagId>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_references_and_cycles_are_rejected() {
        for entries in [
            vec![("a", Some("tag(\"missing\")"))],
            vec![("a", Some("tag(\"a\")"))],
            vec![("a", Some("tag(\"b\")")), ("b", Some("tag(\"a\")"))],
            vec![("a", Some("tag(if(raw > 0, \"a\", \"b\"))"))],
        ] {
            assert!(DependencyGraph::build(entries).is_err());
        }
    }
    #[test]
    fn deep_chain_does_not_use_recursive_graph_traversal() {
        let names: Vec<_> = (0..5000).map(|i| format!("t{i}")).collect();
        let formulas: Vec<_> = (0..5000)
            .map(|i| {
                if i == 0 {
                    None
                } else {
                    Some(format!("tag(\"t{}\") + 1", i - 1))
                }
            })
            .collect();
        let graph = DependencyGraph::build(
            names
                .iter()
                .zip(&formulas)
                .map(|(n, f)| (n.as_str(), f.as_deref())),
        )
        .unwrap();
        assert_eq!(graph.order.len(), 5000);
        assert!(graph.affected(&[TagId(0)]).into_iter().all(|x| x));
    }
}

impl DependencyGraph {
    pub fn build<'a>(
        entries: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    ) -> Result<Self, String> {
        let entries: Vec<_> = entries.into_iter().collect();
        let names: HashMap<_, _> = entries
            .iter()
            .enumerate()
            .map(|(i, (n, _))| (*n, TagId(i as u32)))
            .collect();
        let mut dependencies = vec![Vec::new(); entries.len()];
        let mut dependents = vec![Vec::new(); entries.len()];
        for (i, &(name, formula)) in entries.iter().enumerate() {
            if let Some(formula) = formula {
                for dep in crate::formula::dependencies(formula)
                    .map_err(|e| format!("tag `{name}`: {e}"))?
                {
                    let id = *names
                        .get(dep.as_str())
                        .ok_or_else(|| format!("tag `{name}`: unknown dependency `{dep}`"))?;
                    dependencies[i].push(id);
                    dependents[id.0 as usize].push(TagId(i as u32));
                }
            }
        }
        let mut degrees: Vec<_> = dependencies.iter().map(Vec::len).collect();
        let mut ready: VecDeque<_> = degrees
            .iter()
            .enumerate()
            .filter(|(_, n)| **n == 0)
            .map(|(i, _)| TagId(i as u32))
            .collect();
        let mut order = Vec::with_capacity(entries.len());
        while let Some(tag) = ready.pop_front() {
            order.push(tag);
            for &next in &dependents[tag.0 as usize] {
                degrees[next.0 as usize] -= 1;
                if degrees[next.0 as usize] == 0 {
                    ready.push_back(next);
                }
            }
        }
        if order.len() != entries.len() {
            let blocked: Vec<_> = entries
                .iter()
                .enumerate()
                .filter(|(i, _)| degrees[*i] > 0)
                .map(|(_, (n, _))| *n)
                .collect();
            return Err(format!(
                "formula dependency cycle (including blocked dependents): {}",
                blocked.join(", ")
            ));
        }
        Ok(Self {
            order,
            dependencies,
            dependents,
        })
    }

    pub fn affected(&self, changed: &[TagId]) -> Vec<bool> {
        let mut affected = vec![false; self.order.len()];
        let mut todo = Vec::new();
        for &tag in changed {
            if let Some(flag) = affected.get_mut(tag.0 as usize) {
                if !*flag {
                    *flag = true;
                    todo.push(tag);
                }
            }
        }
        while let Some(tag) = todo.pop() {
            for &next in &self.dependents[tag.0 as usize] {
                if !affected[next.0 as usize] {
                    affected[next.0 as usize] = true;
                    todo.push(next);
                }
            }
        }
        affected
    }
}
