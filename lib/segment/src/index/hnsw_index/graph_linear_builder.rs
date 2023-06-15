use std::collections::BinaryHeap;

use itertools::Itertools;
use rand::distributions::Uniform;
use rand::Rng;

use super::entry_points::EntryPoints;
use super::graph_layers::LinkContainer;
use super::point_scorer::FilteredScorer;
use super::search_context::SearchContext;
use crate::common::utils::rev_range;
use crate::index::visited_pool::VisitedPool;
use crate::spaces::tools::FixedLengthPriorityQueue;
use crate::types::PointOffsetType;
use crate::vector_storage::ScoredPointOffset;

pub type LayersContainer = Vec<LinkContainer>;

pub struct GraphLinearBuilder {
    max_level: usize,
    m: usize,
    m0: usize,
    ef_construct: usize,
    level_factor: f64,
    use_heuristic: bool,
    links_layers: Vec<LayersContainer>,
    entry_points: EntryPoints,
    visited_pool: VisitedPool,
}

impl GraphLinearBuilder {
    pub fn new(
        num_vectors: usize, // Initial number of points in index
        m: usize,           // Expected M for non-first layer
        m0: usize,          // Expected M for first layer
        ef_construct: usize,
        entry_points_num: usize, // Depends on number of points
        use_heuristic: bool,
        reserve: bool,
    ) -> Self {
        let mut links_layers: Vec<LayersContainer> = vec![];

        for _i in 0..num_vectors {
            let mut links = Vec::new();
            if reserve {
                links.reserve(m0);
            }
            links_layers.push(vec![links]);
        }

        Self {
            max_level: 0,
            m,
            m0,
            ef_construct,
            level_factor: 1.0 / (std::cmp::max(m, 2) as f64).ln(),
            use_heuristic,
            links_layers,
            entry_points: EntryPoints::new(entry_points_num),
            visited_pool: VisitedPool::new(),
        }
    }

    pub fn search(
        &self,
        top: usize,
        ef: usize,
        mut points_scorer: FilteredScorer,
    ) -> Vec<ScoredPointOffset> {
        let entry_point = match self
            .entry_points
            .get_entry_point(|point_id| points_scorer.check_vector(point_id))
        {
            None => return vec![],
            Some(ep) => ep,
        };

        let zero_level_entry = self.search_entry(
            entry_point.point_id,
            entry_point.level,
            0,
            &mut points_scorer,
        );

        let nearest = self.search_on_level(
            zero_level_entry,
            0,
            std::cmp::max(top, ef),
            &mut points_scorer,
            &[],
        );
        nearest.into_iter().take(top).collect_vec()
    }

    pub fn link_new_point(&mut self, point_id: PointOffsetType, mut points_scorer: FilteredScorer) {
        // Check if there is an suitable entry point
        //   - entry point level if higher or equal
        //   - it satisfies filters

        let level = self.get_point_level(point_id);

        let entry_point_opt = self.entry_points.new_point(point_id, level, |point_id| {
            points_scorer.check_vector(point_id)
        });
        match entry_point_opt {
            // New point is a new empty entry (for this filter, at least)
            // We can't do much here, so just quit
            None => {}

            // Entry point found.
            Some(entry_point) => {
                let mut level_entry = if entry_point.level > level {
                    // The entry point is higher than a new point
                    // Let's find closest one on same level

                    // greedy search for a single closest point
                    self.search_entry(
                        entry_point.point_id,
                        entry_point.level,
                        level,
                        &mut points_scorer,
                    )
                } else {
                    ScoredPointOffset {
                        idx: entry_point.point_id,
                        score: points_scorer.score_internal(point_id, entry_point.point_id),
                    }
                };
                // minimal common level for entry points
                let linking_level = std::cmp::min(level, entry_point.level);

                for curr_level in (0..=linking_level).rev() {
                    let level_m = self.get_m(curr_level);

                    let nearest_points = {
                        let existing_links = &self.links_layers[point_id as usize][curr_level];
                        self.search_on_level(
                            level_entry,
                            curr_level,
                            self.ef_construct,
                            &mut points_scorer,
                            &existing_links,
                        )
                    };

                    if let Some(the_nearest) = nearest_points.iter().max() {
                        level_entry = *the_nearest;
                    }

                    if self.use_heuristic {
                        let selected_nearest = Self::select_candidates_with_heuristic(
                            nearest_points,
                            level_m,
                            &mut points_scorer,
                        );
                        self.links_layers[point_id as usize][curr_level]
                            .clone_from(&selected_nearest);

                        for &other_point in &selected_nearest {
                            let other_point_links =
                                &mut self.links_layers[other_point as usize][curr_level];
                            if other_point_links.len() < level_m {
                                // If linked point is lack of neighbours
                                other_point_links.push(point_id);
                            } else {
                                let mut candidates = BinaryHeap::with_capacity(level_m + 1);
                                candidates.push(ScoredPointOffset {
                                    idx: point_id,
                                    score: points_scorer.score_internal(point_id, other_point),
                                });
                                for other_point_link in
                                    other_point_links.iter().take(level_m).copied()
                                {
                                    candidates.push(ScoredPointOffset {
                                        idx: other_point_link,
                                        score: points_scorer
                                            .score_internal(other_point_link, other_point),
                                    });
                                }
                                let selected_candidates =
                                    Self::select_candidate_with_heuristic_from_sorted(
                                        candidates.into_sorted_vec().into_iter().rev(),
                                        level_m,
                                        &mut points_scorer,
                                    );
                                other_point_links.clear(); // this do not free memory, which is good
                                for selected in selected_candidates.iter().copied() {
                                    other_point_links.push(selected);
                                }
                            }
                        }
                    } else {
                        for nearest_point in &nearest_points {
                            {
                                let links = &mut self.links_layers[point_id as usize][curr_level];
                                Self::connect_new_point(
                                    links,
                                    nearest_point.idx,
                                    point_id,
                                    level_m,
                                    &mut points_scorer,
                                );
                            }

                            {
                                let links =
                                    &mut self.links_layers[nearest_point.idx as usize][curr_level];
                                Self::connect_new_point(
                                    links,
                                    point_id,
                                    nearest_point.idx,
                                    level_m,
                                    &mut points_scorer,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Connect new point to links, so that links contains only closest points
    fn connect_new_point(
        links: &mut LinkContainer,
        new_point_id: PointOffsetType,
        target_point_id: PointOffsetType,
        level_m: usize,
        points_scorer: &mut FilteredScorer,
    ) {
        // ToDo: binary search here ? (most likely does not worth it)
        let new_to_target = points_scorer.score_internal(target_point_id, new_point_id);

        let mut id_to_insert = links.len();
        for (i, &item) in links.iter().enumerate() {
            let target_to_link = points_scorer.score_internal(target_point_id, item);
            if target_to_link < new_to_target {
                id_to_insert = i;
                break;
            }
        }

        if links.len() < level_m {
            links.insert(id_to_insert, new_point_id);
        } else if id_to_insert != links.len() {
            links.pop();
            links.insert(id_to_insert, new_point_id);
        }
    }

    /// <https://github.com/nmslib/hnswlib/issues/99>
    fn select_candidate_with_heuristic_from_sorted(
        candidates: impl Iterator<Item = ScoredPointOffset>,
        m: usize,
        points_scorer: &mut FilteredScorer,
    ) -> Vec<PointOffsetType> {
        let mut result_list = vec![];
        result_list.reserve(m);
        for current_closest in candidates {
            if result_list.len() >= m {
                break;
            }
            let mut is_good = true;
            for &selected_point in &result_list {
                let dist_to_already_selected =
                    points_scorer.score_internal(current_closest.idx, selected_point);
                if dist_to_already_selected > current_closest.score {
                    is_good = false;
                    break;
                }
            }
            if is_good {
                result_list.push(current_closest.idx);
            }
        }

        result_list
    }

    /// <https://github.com/nmslib/hnswlib/issues/99>
    fn select_candidates_with_heuristic(
        candidates: FixedLengthPriorityQueue<ScoredPointOffset>,
        m: usize,
        points_scorer: &mut FilteredScorer,
    ) -> Vec<PointOffsetType> {
        let closest_iter = candidates.into_iter();
        Self::select_candidate_with_heuristic_from_sorted(closest_iter, m, points_scorer)
    }

    fn search_on_level(
        &self,
        level_entry: ScoredPointOffset,
        level: usize,
        ef: usize,
        points_scorer: &mut FilteredScorer,
        existing_links: &[PointOffsetType],
    ) -> FixedLengthPriorityQueue<ScoredPointOffset> {
        let mut visited_list = self.visited_pool.get(self.links_layers.len());
        visited_list.check_and_update_visited(level_entry.idx);
        let mut searcher = SearchContext::new(level_entry, ef);

        let limit = self.get_m(level);
        let mut points_ids: Vec<PointOffsetType> = Vec::with_capacity(2 * limit);

        while let Some(candidate) = searcher.candidates.pop() {
            if candidate.score < searcher.lower_bound() {
                break;
            }

            points_ids.clear();
            self.links_map(candidate.idx, level, |link| {
                if !visited_list.check_and_update_visited(link) {
                    points_ids.push(link);
                }
            });

            let scores = points_scorer.score_points(&mut points_ids, limit);
            scores
                .iter()
                .copied()
                .for_each(|score_point| searcher.process_candidate(score_point));
        }

        for &existing_link in existing_links {
            if !visited_list.check(existing_link) {
                searcher.process_candidate(ScoredPointOffset {
                    idx: existing_link,
                    score: points_scorer.score_point(existing_link),
                });
            }
        }

        self.visited_pool.return_back(visited_list);
        searcher.nearest
    }

    fn search_entry(
        &self,
        entry_point: PointOffsetType,
        top_level: usize,
        target_level: usize,
        points_scorer: &mut FilteredScorer,
    ) -> ScoredPointOffset {
        let mut links: Vec<PointOffsetType> = Vec::with_capacity(2 * self.get_m(0));

        let mut current_point = ScoredPointOffset {
            idx: entry_point,
            score: points_scorer.score_point(entry_point),
        };
        for level in rev_range(top_level, target_level) {
            let limit = self.get_m(level);

            let mut changed = true;
            while changed {
                changed = false;

                links.clear();
                self.links_map(current_point.idx, level, |link| {
                    links.push(link);
                });

                let scores = points_scorer.score_points(&mut links, limit);
                scores.iter().copied().for_each(|score_point| {
                    if score_point.score > current_point.score {
                        changed = true;
                        current_point = score_point;
                    }
                });
            }
        }
        current_point
    }

    fn get_m(&self, level: usize) -> usize {
        if level == 0 {
            self.m0
        } else {
            self.m
        }
    }

    fn links_map<F>(&self, point_id: PointOffsetType, level: usize, mut f: F)
    where
        F: FnMut(PointOffsetType),
    {
        let links = &self.links_layers[point_id as usize][level];
        for link in links.iter() {
            f(*link);
        }
    }

    /// Generate random level for a new point, according to geometric distribution
    pub fn get_random_layer<R>(&self, rng: &mut R) -> usize
    where
        R: Rng + ?Sized,
    {
        let distribution = Uniform::new(0.0, 1.0);
        let sample: f64 = rng.sample(distribution);
        let picked_level = -sample.ln() * self.level_factor;
        picked_level.round() as usize
    }

    fn get_point_level(&self, point_id: PointOffsetType) -> usize {
        self.links_layers[point_id as usize].len() - 1
    }

    pub fn set_levels(&mut self, point_id: PointOffsetType, level: usize) {
        if self.links_layers.len() <= point_id as usize {
            while self.links_layers.len() <= point_id as usize {
                self.links_layers.push(vec![]);
            }
        }
        let point_layers = &mut self.links_layers[point_id as usize];
        while point_layers.len() <= level {
            let mut links = vec![];
            links.reserve(self.m);
            point_layers.push(links);
        }
        self.max_level = std::cmp::max(self.max_level, level);
    }
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    use super::*;
    use crate::fixtures::index_fixtures::{FakeFilterContext, TestRawScorerProducer};
    use crate::index::hnsw_index::graph_layers_builder::GraphLayersBuilder;
    use crate::index::hnsw_index::point_scorer::FilteredScorer;
    use crate::spaces::simple::CosineMetric;
    use crate::types::PointOffsetType;

    const M: usize = 8;

    #[test]
    fn test_equal_hnsw() {
        let num_vectors = 1000;
        let m = M;
        let ef_construct = 16;
        let entry_points_num = 10;

        let mut rng = StdRng::seed_from_u64(42);
        let vector_holder = TestRawScorerProducer::<CosineMetric>::new(16, num_vectors, &mut rng);

        let mut graph_layers_1 = GraphLayersBuilder::new_with_params(
            num_vectors,
            m,
            m * 2,
            ef_construct,
            entry_points_num,
            true,
            true,
        );
        let mut graph_layers_2 = GraphLinearBuilder::new(
            num_vectors,
            m,
            m * 2,
            ef_construct,
            entry_points_num,
            true,
            true,
        );

        for idx in 0..(num_vectors as PointOffsetType) {
            let level = graph_layers_1.get_random_layer(&mut rng);
            graph_layers_1.set_levels(idx, level);
            graph_layers_2.set_levels(idx, level);
        }

        for idx in 0..(num_vectors as PointOffsetType) {
            let fake_filter_context = FakeFilterContext {};
            let added_vector = vector_holder.vectors.get(idx).to_vec();
            let raw_scorer = vector_holder.get_raw_scorer(added_vector.clone());

            let scorer = FilteredScorer::new(raw_scorer.as_ref(), Some(&fake_filter_context));
            graph_layers_1.link_new_point(idx, scorer);

            let scorer = FilteredScorer::new(raw_scorer.as_ref(), Some(&fake_filter_context));
            graph_layers_2.link_new_point(idx, scorer);
        }

        assert_eq!(
            graph_layers_1.links_layers.len(),
            graph_layers_2.links_layers.len(),
        );
        for (links_1, links_2) in graph_layers_1
            .links_layers
            .iter()
            .zip(graph_layers_2.links_layers.iter())
        {
            assert_eq!(links_1.len(), links_2.len());
            for (links_1, links_2) in links_1.iter().zip(links_2.iter()) {
                assert_eq!(links_1.read().clone(), links_2.clone());
            }
        }
    }
}
