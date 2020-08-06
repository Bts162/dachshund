/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */
extern crate clap;
extern crate serde_json;

use crate::dachshund::error::CLQResult;
use crate::dachshund::graph_base::GraphBase;
use crate::dachshund::id_types::{GraphId, NodeId};
use crate::dachshund::input::Input;
use crate::dachshund::line_processor::LineProcessor;
use crate::dachshund::output::Output;
use crate::dachshund::row::{Row, SimpleEdgeRow};
use crate::dachshund::simple_undirected_graph::SimpleUndirectedGraph;
use crate::dachshund::simple_undirected_graph_builder::SimpleUndirectedGraphBuilder;
use rand::seq::SliceRandom;
use rayon::{ThreadPool, ThreadPoolBuilder};
use serde_json::json;
use std::collections::HashSet;
use std::io::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub struct SimpleTransformer {
    batch: Vec<SimpleEdgeRow>,
    line_processor: Arc<LineProcessor>,
}
pub struct SimpleParallelTransformer {
    batch: Vec<SimpleEdgeRow>,
    pool: ThreadPool,
    line_processor: Arc<LineProcessor>,
}
pub trait TransformerBase {
    fn get_line_processor(&self) -> Arc<LineProcessor>;
    // logic for taking row and storing into self via side-effect
    fn process_row(&mut self, row: Box<dyn Row>) -> CLQResult<()>;
    // logic for processing batch of rows, once all rows are ready
    fn process_batch(&self, graph_id: GraphId, output: &Sender<(String, bool)>) -> CLQResult<()>;
    // reset transformer state after processing;
    fn reset(&mut self) -> CLQResult<()>;

    // main loop, runs through lines ordered by graph_id, updates state accordingly
    // and runs process_batch when graph_id changes
    fn run(&mut self, input: Input, mut output: Output) -> CLQResult<()> {
        let ret = crossbeam::scope(|scope| {
            let line_processor = self.get_line_processor();
            let num_processed = Arc::new(AtomicUsize::new(0 as usize));
            let (sender, receiver) = channel();
            let num_processed_clone = num_processed.clone();
            let writer = scope.spawn(move |_| loop {
                match receiver.recv() {
                    Ok((line, shutdown)) => {
                        if shutdown {
                            return;
                        }
                        output.print(line).unwrap();
                        num_processed_clone.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(error) => panic!(error),
                }
            });
            let mut current_graph_id: Option<GraphId> = None;
            let mut num_to_process: usize = 0;
            for line in input.lines() {
                match line {
                    Ok(n) => {
                        let row: Box<dyn Row> = line_processor.process_line(n)?;
                        let new_graph_id: GraphId = row.get_graph_id();
                        if let Some(some_current_graph_id) = current_graph_id {
                            if new_graph_id != some_current_graph_id {
                                self.process_batch(some_current_graph_id, &sender.clone())?;
                                num_to_process += 1;
                                self.reset()?;
                            }
                        }
                        current_graph_id = Some(new_graph_id);
                        self.process_row(row)?;
                    }
                    Err(error) => eprintln!("I/O error: {}", error),
                }
            }
            if let Some(some_current_graph_id) = current_graph_id {
                self.process_batch(some_current_graph_id, &sender)?;
                num_to_process += 1;
                while num_to_process > num_processed.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(100));
                }
                sender.send(("".to_string(), true)).unwrap();
                writer.join().unwrap();
                return Ok(());
            }
            Err("No input rows!".into())
        });
        ret.unwrap()
    }
}

pub trait GraphStatsTransformerBase: TransformerBase {
    fn compute_graph_stats_json(graph: &SimpleUndirectedGraph) -> String {
        let conn_comp = graph.get_connected_components();
        let largest_cc = conn_comp.iter().max_by_key(|x| x.len()).unwrap();
        let sources: Vec<NodeId> = largest_cc
            .choose_multiple(&mut rand::thread_rng(), 100)
            .copied()
            .collect();
        let betcent = graph
            .get_node_betweenness_starting_from_sources(&sources, false, Some(&largest_cc))
            .unwrap();
        let evcent = graph.get_eigenvector_centrality(0.001, 1000);

        let mut removed: HashSet<NodeId> = HashSet::new();
        let k_cores_2 = graph._get_k_cores(2, &mut removed);
        let k_trusses_3 = graph._get_k_trusses(3, &removed).1;
        let k_cores_4 = graph._get_k_cores(4, &mut removed);
        let k_trusses_5 = graph._get_k_trusses(5, &removed).1;
        let k_cores_8 = graph._get_k_cores(8, &mut removed);
        let k_trusses_9 = graph._get_k_trusses(9, &removed).1;
        let k_cores_16 = graph._get_k_cores(16, &mut removed);
        let k_trusses_17 = graph._get_k_trusses(17, &removed).1;

        json!({
            "num_edges": graph.count_edges(),
            "num_2_cores": k_cores_2.len(),
            "num_4_cores": k_cores_4.len(),
            "num_8_cores": k_cores_8.len(),
            "num_16_cores": k_cores_16.len(),
            "num_3_trusses": k_trusses_3.len(),
            "num_5_trusses": k_trusses_5.len(),
            "num_9_trusses": k_trusses_9.len(),
            "num_17_trusses": k_trusses_17.len(),
            "num_connected_components": conn_comp.len(),
            "size_of_largest_cc": largest_cc.len(),
            "bet_cent": (Iterator::sum::<f64>(betcent.values()) /
                (betcent.len() as f64) * 1000.0).floor() / 1000.0,
            "evcent": (Iterator::sum::<f64>(evcent.values()) /
                (evcent.len() as f64) * 1000.0).floor() / 1000.0,
            "clust_coef": (graph.get_avg_clustering() * 1000.0).floor() / 1000.0,
        })
        .to_string()
    }
}
impl SimpleTransformer {
    pub fn new() -> Self {
        Self {
            batch: Vec::new(),
            line_processor: Arc::new(LineProcessor::new()),
        }
    }
}
impl Default for SimpleTransformer {
    fn default() -> Self {
        SimpleTransformer::new()
    }
}
impl SimpleParallelTransformer {
    pub fn new() -> Self {
        Self {
            batch: Vec::new(),
            line_processor: Arc::new(LineProcessor::new()),
            pool: ThreadPoolBuilder::new().build().unwrap(),
        }
    }
}
impl Default for SimpleParallelTransformer {
    fn default() -> Self {
        SimpleParallelTransformer::new()
    }
}

impl TransformerBase for SimpleTransformer {
    fn get_line_processor(&self) -> Arc<LineProcessor> {
        self.line_processor.clone()
    }
    fn process_row(&mut self, row: Box<dyn Row>) -> CLQResult<()> {
        self.batch.push(row.as_simple_edge_row().unwrap());
        Ok(())
    }
    fn reset(&mut self) -> CLQResult<()> {
        self.batch.clear();
        Ok(())
    }
    fn process_batch(&self, graph_id: GraphId, output: &Sender<(String, bool)>) -> CLQResult<()> {
        let tuples: Vec<(i64, i64)> = self.batch.iter().map(|x| x.as_tuple()).collect();
        let graph = SimpleUndirectedGraphBuilder::from_vector(&tuples);
        let stats = Self::compute_graph_stats_json(&graph);
        let original_id = self
            .line_processor
            .get_original_id(graph_id.value() as usize);
        let line: String = format!("{}\t{}", original_id, stats);
        output.send((line, false)).unwrap();
        Ok(())
    }
}
impl TransformerBase for SimpleParallelTransformer {
    fn get_line_processor(&self) -> Arc<LineProcessor> {
        self.line_processor.clone()
    }
    fn process_row(&mut self, row: Box<dyn Row>) -> CLQResult<()> {
        self.batch.push(row.as_simple_edge_row().unwrap());
        Ok(())
    }
    fn reset(&mut self) -> CLQResult<()> {
        self.batch.clear();
        Ok(())
    }
    fn process_batch(&self, graph_id: GraphId, output: &Sender<(String, bool)>) -> CLQResult<()> {
        let tuples: Vec<(i64, i64)> = self.batch.iter().map(|x| x.as_tuple()).collect();
        let output_clone = output.clone();
        let line_processor = self.line_processor.clone();
        self.pool.spawn(move || {
            let graph = SimpleUndirectedGraphBuilder::from_vector(&tuples);
            let stats = Self::compute_graph_stats_json(&graph);
            let original_id = line_processor.get_original_id(graph_id.value() as usize);
            let line: String = format!("{}\t{}", original_id, stats);
            output_clone.send((line, false)).unwrap();
        });
        Ok(())
    }
}
impl GraphStatsTransformerBase for SimpleTransformer {}
impl GraphStatsTransformerBase for SimpleParallelTransformer {}
