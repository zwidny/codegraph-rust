// ABOUTME: Pure Rust k-means clustering lifted out of `fn::kmeans`.
// ABOUTME: Deterministic seeding and tie-breaking; no database access.

//! Pure k-means clustering that used to live in `schema/codegraph_v2.surql` as
//! `fn::kmeans`.
//!
//! The stored procedure is a Lloyd's algorithm with deterministic seeds: cluster `i` starts
//! at `rows[i * floor(n / clusters)]`. Every round assigns each row to the nearest centroid
//! by Euclidean distance (ties keep the first centroid) and recomputes each centroid as the
//! mean of its members. A cluster that receives no members keeps its previous centroid.
//!
//! Moving this here makes the rule covered by ordinary unit tests, and keeps the result
//! independent of datastore array-size limits.

/// One row fed to [`kmeans`], carrying the id reported back in [`Cluster::members`].
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterInput {
    pub id: String,
    pub vector: Vec<f32>,
}

/// One output cluster, mirroring the object `fn::kmeans` returns.
///
/// `members` preserves the input order of `rows`, because the stored procedure filters the
/// assignment array rather than sorting it.
#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    pub cluster: usize,
    pub centroid: Vec<f32>,
    pub members: Vec<String>,
}

/// Euclidean distance, accumulated in f64 so long vectors do not lose precision.
fn euclidean(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let delta = f64::from(*x) - f64::from(*y);
            delta * delta
        })
        .sum::<f64>()
        .sqrt()
}

/// Lloyd's k-means with the exact defaults of `fn::kmeans($rows, $k, $iterations)`.
///
/// - `n == 0` returns an empty vector.
/// - `clusters` is `k` when `0 < k < n`, otherwise `max(1, min(n, 2))`.
/// - `rounds` is `iterations` when `0 < iterations <= 50`, otherwise `10`.
/// - seed `i` is `rows[i * floor(n / clusters)].vector`.
/// - each round assigns by nearest Euclidean distance, ties to the first centroid, then
///   replaces each centroid by the mean of its members; an empty cluster keeps its old
///   centroid.
///
/// The result is deterministic: the same input always produces the same clusters.
pub fn kmeans(rows: &[ClusterInput], k: usize, iterations: usize) -> Vec<Cluster> {
    let n = rows.len();
    if n == 0 {
        return Vec::new();
    }

    // `IF $k > 0 AND $k < $n THEN $k ELSE math::max([1, math::min([$n, 2])]) END`.
    let clusters = if k > 0 && k < n { k } else { 1.max(n.min(2)) };
    // `IF $iterations > 0 AND $iterations <= 50 THEN $iterations ELSE 10 END`.
    let rounds = if iterations > 0 && iterations <= 50 {
        iterations
    } else {
        10
    };

    // Uniformly spaced deterministic seeds. `clusters <= n` keeps `i * step` in range.
    let step = n / clusters;
    let mut centroids: Vec<Vec<f32>> = (0..clusters)
        .map(|i| rows[i * step].vector.clone())
        .collect();
    let mut assignment: Vec<usize> = vec![0; n];

    for _ in 0..rounds {
        for (index, row) in rows.iter().enumerate() {
            let mut best = 0;
            let mut best_distance = f64::INFINITY;
            for (cluster, centroid) in centroids.iter().enumerate() {
                let distance = euclidean(&row.vector, centroid);
                // Strictly less than: `array::find_index` keeps the first minimum.
                if distance < best_distance {
                    best_distance = distance;
                    best = cluster;
                }
            }
            assignment[index] = best;
        }

        let mut next: Vec<Vec<f32>> = Vec::with_capacity(clusters);
        for cluster in 0..clusters {
            let mut count = 0usize;
            let mut sums = vec![0.0f64; centroids[cluster].len()];
            for (index, assigned) in assignment.iter().enumerate() {
                if *assigned != cluster {
                    continue;
                }
                count += 1;
                for (dim, value) in rows[index].vector.iter().enumerate() {
                    if let Some(sum) = sums.get_mut(dim) {
                        *sum += f64::from(*value);
                    }
                }
            }
            if count == 0 {
                // `THEN $state.centroids[$c]`: an empty cluster keeps its old centroid.
                next.push(centroids[cluster].clone());
            } else {
                next.push(
                    sums.into_iter()
                        .map(|sum| (sum / count as f64) as f32)
                        .collect(),
                );
            }
        }
        centroids = next;
    }

    (0..clusters)
        .map(|cluster| Cluster {
            cluster,
            centroid: centroids[cluster].clone(),
            members: assignment
                .iter()
                .enumerate()
                .filter(|(_, assigned)| **assigned == cluster)
                .map(|(index, _)| rows[index].id.clone())
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, vector: &[f32]) -> ClusterInput {
        ClusterInput {
            id: id.to_string(),
            vector: vector.to_vec(),
        }
    }

    /// The differential-test fixture: r0-r2 near the origin, r3-r5 near (10, 10).
    fn fixture() -> Vec<ClusterInput> {
        (0..6)
            .map(|i| {
                if i < 3 {
                    row(&format!("r{i}"), &[i as f32 * 0.01, 0.0])
                } else {
                    row(&format!("r{i}"), &[10.0 + i as f32 * 0.01, 10.0])
                }
            })
            .collect()
    }

    fn members(clusters: &[Cluster]) -> Vec<Vec<String>> {
        clusters.iter().map(|c| c.members.clone()).collect()
    }

    #[test]
    fn kmeans_on_an_empty_input_returns_nothing() {
        assert!(kmeans(&[], 2, 5).is_empty());
        assert!(kmeans(&[], 0, 0).is_empty());
    }

    #[test]
    fn kmeans_on_a_single_row_returns_one_cluster() {
        let rows = vec![row("only", &[1.0, 2.0])];
        for k in [0usize, 1, 2, 99] {
            let clusters = kmeans(&rows, k, 5);
            assert_eq!(clusters.len(), 1, "k={k}");
            assert_eq!(clusters[0].cluster, 0);
            assert_eq!(clusters[0].members, vec!["only".to_string()]);
            assert_eq!(clusters[0].centroid, vec![1.0, 2.0]);
        }
    }

    #[test]
    fn kmeans_falls_back_to_two_clusters_when_k_is_out_of_range() {
        let rows = fixture();
        // k == 0, k == n and k > n all take `max(1, min(n, 2))`.
        for k in [0usize, 6, 99] {
            let clusters = kmeans(&rows, k, 5);
            assert_eq!(clusters.len(), 2, "k={k}");
        }
        // A k inside `0 < k < n` is honoured, even at the floor of one cluster.
        assert_eq!(kmeans(&rows, 1, 5).len(), 1);
        // n == 2: min(n, 2) is 2, so the default is still two clusters.
        let two = vec![row("a", &[0.0]), row("b", &[10.0])];
        assert_eq!(kmeans(&two, 5, 5).len(), 2);
    }

    #[test]
    fn kmeans_splits_two_well_separated_groups() {
        let clusters = kmeans(&fixture(), 2, 5);
        assert_eq!(
            members(&clusters),
            vec![
                vec!["r0".to_string(), "r1".to_string(), "r2".to_string()],
                vec!["r3".to_string(), "r4".to_string(), "r5".to_string()],
            ]
        );
        // Centroids are the means of each group.
        assert!((clusters[0].centroid[0] - 0.01).abs() < 1e-6);
        assert!((clusters[0].centroid[1] - 0.0).abs() < 1e-6);
        assert!((clusters[1].centroid[0] - 10.04).abs() < 1e-5);
        assert!((clusters[1].centroid[1] - 10.0).abs() < 1e-6);
    }

    #[test]
    fn kmeans_uses_ten_rounds_when_iterations_is_out_of_range() {
        let rows = fixture();
        let ten = kmeans(&rows, 2, 10);
        assert_eq!(kmeans(&rows, 2, 0), ten, "0 must fall back to 10 rounds");
        assert_eq!(kmeans(&rows, 2, 51), ten, "51 must fall back to 10 rounds");
        // 1 and 50 are honoured, not replaced.
        assert_eq!(kmeans(&rows, 2, 1).len(), 2);
        assert_eq!(kmeans(&rows, 2, 50).len(), 2);
        // The fixture converges immediately, so the round count cannot change the split.
        assert_eq!(kmeans(&rows, 2, 1), ten);
    }

    #[test]
    fn kmeans_breaks_distance_ties_towards_the_first_centroid() {
        // Seeds are rows[0] = [0] and rows[1] = [2] (step = 1). r2 = [1] is exactly
        // midway, so it must join cluster 0, like `array::find_index(min)`.
        let rows = vec![row("r0", &[0.0]), row("r1", &[2.0]), row("r2", &[1.0])];
        let clusters = kmeans(&rows, 2, 1);
        assert_eq!(
            members(&clusters),
            vec![
                vec!["r0".to_string(), "r2".to_string()],
                vec!["r1".to_string()]
            ]
        );
    }

    #[test]
    fn kmeans_keeps_the_previous_centroid_for_an_empty_cluster() {
        // Seeds are rows[0] = [0] and rows[1] = [0] (step = 1). Both centroids coincide,
        // so every row ties towards cluster 0 and cluster 1 stays empty after round one.
        // A single round is deliberate: once cluster 0's centroid moves towards the mean,
        // later rounds would pull the origin rows back into cluster 1.
        let rows = vec![row("r0", &[0.0]), row("r1", &[0.0]), row("r2", &[10.0])];
        let clusters = kmeans(&rows, 2, 1);
        assert_eq!(
            members(&clusters),
            vec![
                vec!["r0".to_string(), "r1".to_string(), "r2".to_string()],
                Vec::<String>::new(),
            ]
        );
        // The empty cluster keeps its seed centroid rather than becoming NaN or empty.
        assert_eq!(clusters[1].centroid, vec![0.0]);
    }

    #[test]
    fn kmeans_preserves_input_order_within_a_cluster() {
        // Interleaved input: cluster 0 gets rc, ra; cluster 1 gets rd, rb. Membership must
        // follow the input order, not the assignment discovery order.
        let rows = vec![
            row("rc", &[0.0]),
            row("rd", &[10.0]),
            row("ra", &[0.1]),
            row("rb", &[10.1]),
        ];
        let clusters = kmeans(&rows, 2, 5);
        assert_eq!(
            members(&clusters),
            vec![
                vec!["rc".to_string(), "ra".to_string()],
                vec!["rd".to_string(), "rb".to_string()],
            ]
        );
    }

    #[test]
    fn kmeans_is_deterministic() {
        let rows = fixture();
        assert_eq!(kmeans(&rows, 2, 5), kmeans(&rows, 2, 5));
    }
}
