//! Serial Dual Parallel
//! 
//! A parallel implementation of the dual module, leveraging the serial version
//! 
//! While it assumes single machine (using async runtime of Rust), the design targets distributed version
//! that can spawn on different machines efficiently. The distributed version can be build based on this 
//! parallel version.
//! 
//! Notes:
//! 
//! According to https://tokio.rs/tokio/tutorial, tokio is not good for parallel computation. It suggests
//! using https://docs.rs/rayon/latest/rayon/. 
//!

use super::util::*;
use std::sync::{Arc, Weak};
use super::dual_module::*;
use super::dual_module_serial::*;
use crate::parking_lot::RwLock;
use crate::serde_json;
use serde::{Serialize, Deserialize};
use super::visualize::*;
use crate::rayon::prelude::*;
use std::collections::BTreeSet;
use super::complete_graph::CompleteGraph;


pub struct DualModuleParallel {
    /// initializer, used for customized partition
    pub initializer: SolverInitializer,
    /// the basic wrapped serial modules at the beginning, afterwards the fused units are appended after them
    pub units: Vec<DualModuleParallelUnitPtr>,
    /// configuration
    pub config: DualModuleParallelConfig,
    /// partition information generated by the config
    pub partition_info: PartitionInfo,
    /// thread pool used to execute async functions in parallel
    pub thread_pool: rayon::ThreadPool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DualModuleParallelConfig {
    /// enable async execution of dual operations
    #[serde(default = "dual_module_parallel_default_configs::thread_pool_size")]
    pub thread_pool_size: usize,
    /// detailed plan of partitioning serial modules: each serial module possesses a list of vertices, including all interface vertices
    #[serde(default = "dual_module_parallel_default_configs::partitions")]
    pub partitions: Vec<VertexRange>,
    /// detailed plan of interfacing vertices
    #[serde(default = "dual_module_parallel_default_configs::fusions")]
    pub fusions: Vec<(usize, usize)>,
    /// strategy of edges placement: if edges are placed in the fusion unit, it's good for software implementation because there are no duplicate
    /// edges and no unnecessary vertices in the descendant units. On the other hand, it's not very favorable if implemented on hardware: the 
    /// fusion unit usually contains a very small amount of vertices and edges for the interfacing between two blocks, but maintaining this small graph
    /// may consume additional hardware resources and increase the decoding latency. I want the algorithm to finally work on the hardware efficiently
    /// so I need to verify that it does work by holding all the fusion unit's owned vertices and edges in the descendants, although usually duplicated.
    #[serde(default = "dual_module_parallel_default_configs::edges_in_fusion_unit")]
    pub edges_in_fusion_unit: bool,
}

impl Default for DualModuleParallelConfig {
    fn default() -> Self { serde_json::from_value(json!({})).unwrap() }
}

pub mod dual_module_parallel_default_configs {
    use super::*;
    // pub fn thread_pool_size() -> usize { 0 }  // by default to the number of CPU cores
    pub fn thread_pool_size() -> usize { 1 }  // debug: use a single core
    pub fn partitions() -> Vec<VertexRange> { vec![] }  // by default, this field is optional, and when empty, it will have only 1 partition
    pub fn fusions() -> Vec<(usize, usize)> { vec![] }  // by default no interface
    pub fn edges_in_fusion_unit() -> bool { true }  // by default use the software-friendly approach because of removing duplicate edges
}

pub struct PartitionInfo {
    /// individual info of each unit
    pub units: Vec<PartitionUnitInfo>,
    /// the mapping from vertices to the owning unit: serial unit (holding real vertices) as well as parallel units (holding interfacing vertices);
    /// used for loading syndrome to the holding units
    pub vertex_to_owning_unit: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct PartitionUnitInfo {
    /// the whole range of units
    pub whole_range: VertexRange,
    /// the owning range of units, meaning vertices inside are exclusively belonging to the unit
    pub owning_range: VertexRange,
    /// left and right
    pub children: Option<(usize, usize)>,
    /// parent dual module
    pub parent: Option<usize>,
    /// all the leaf dual modules
    pub leaves: Vec<usize>,
    /// all the descendants
    pub descendants: BTreeSet<usize>,
}

impl PartitionInfo {

    pub fn new(config: &DualModuleParallelConfig, initializer: &SolverInitializer) -> Self {
        assert!(config.partitions.len() > 0, "at least one partition must exist");
        let mut whole_ranges = vec![];
        let mut owning_ranges = vec![];
        for partition in config.partitions.iter() {
            partition.sanity_check();
            assert!(partition.end() <= initializer.vertex_num, "invalid vertex index {} in partitions", partition.end());
            whole_ranges.push(partition.clone());
            owning_ranges.push(partition.clone());
        }
        let mut parents: Vec<Option<usize>> = (0..config.partitions.len() + config.fusions.len()).map(|_| None).collect();
        for (fusion_index, (left_index, right_index)) in config.fusions.iter().enumerate() {
            let unit_index = fusion_index + config.partitions.len();
            assert!(*left_index < unit_index, "dependency wrong, {} depending on {}", unit_index, left_index);
            assert!(*right_index < unit_index, "dependency wrong, {} depending on {}", unit_index, right_index);
            assert!(parents[*left_index].is_none(), "cannot fuse {} twice", left_index);
            assert!(parents[*right_index].is_none(), "cannot fuse {} twice", right_index);
            parents[*left_index] = Some(unit_index);
            parents[*right_index] = Some(unit_index);
            // fusing range
            let (whole_range, interface_range) = whole_ranges[*left_index].fuse(&whole_ranges[*right_index]);
            whole_ranges.push(whole_range);
            owning_ranges.push(interface_range);
        }
        // check that all nodes except for the last one has been merged
        for unit_index in 0..config.partitions.len() + config.fusions.len() - 1 {
            assert!(parents[unit_index].is_some(), "found unit {} without being fused", unit_index);
        }
        // check that the final node has the full range
        let last_unit_index = config.partitions.len() + config.fusions.len() - 1;
        assert!(whole_ranges[last_unit_index].start() == 0, "final range not covering all vertices {:?}", whole_ranges[last_unit_index]);
        assert!(whole_ranges[last_unit_index].end() == initializer.vertex_num, "final range not covering all vertices {:?}", whole_ranges[last_unit_index]);
        // construct partition info
        let mut partition_unit_info: Vec<_> = (0..config.partitions.len() + config.fusions.len()).map(|i| {
            PartitionUnitInfo {
                whole_range: whole_ranges[i],
                owning_range: owning_ranges[i],
                children: if i >= config.partitions.len() { Some(config.fusions[i - config.partitions.len()]) } else { None },
                parent: parents[i].clone(),
                leaves: if i < config.partitions.len() { vec![i] } else { vec![] },
                descendants: BTreeSet::new(),
            }
        }).collect();
        // build descendants
        for (fusion_index, (left_index, right_index)) in config.fusions.iter().enumerate() {
            let unit_index = fusion_index + config.partitions.len();
            let mut leaves = vec![];
            leaves.extend(partition_unit_info[*left_index].leaves.iter());
            leaves.extend(partition_unit_info[*right_index].leaves.iter());
            partition_unit_info[unit_index].leaves.extend(leaves.iter());
            let mut descendants = vec![];
            descendants.push(*left_index);
            descendants.push(*right_index);
            descendants.extend(partition_unit_info[*left_index].descendants.iter());
            descendants.extend(partition_unit_info[*right_index].descendants.iter());
            partition_unit_info[unit_index].descendants.extend(descendants.iter());
        }
        let mut vertex_to_owning_unit: Vec<_> = (0..initializer.vertex_num).map(|_| usize::MAX).collect();
        for (unit_index, unit_range) in partition_unit_info.iter().map(|x| x.owning_range).enumerate() {
            for vertex_index in unit_range.iter() {
                vertex_to_owning_unit[vertex_index] = unit_index;
            }
        }
        Self {
            units: partition_unit_info,
            vertex_to_owning_unit: vertex_to_owning_unit,
        }
    }

}

pub struct DualModuleParallelUnit {
    /// fused module is not accessible globally: it must be accessed from its parent
    pub is_fused: bool,
    /// whether it's active or not; some units are "placeholder" units that are not active until they actually fuse their children
    pub is_active: bool,
    /// the vertex range of this parallel unit consists of all the owning_range of its descendants
    pub whole_range: VertexRange,
    /// the vertices owned by this unit, note that owning_range is a subset of whole_range
    pub owning_range: VertexRange,
    /// `Some(_)` only if this parallel dual module is a simple wrapper of a serial dual module
    pub serial_module: DualModuleSerialPtr,
    /// left and right children dual modules
    pub children: Option<(DualModuleParallelUnitWeak, DualModuleParallelUnitWeak)>,
    /// parent dual module
    pub parent: Option<DualModuleParallelUnitWeak>,
    /// interfacing nodes between the left and right
    pub nodes: Vec<Option<DualNodeInternalPtr>>,
    /// interface ids (each dual module may have multiple interfaces, e.g. in case A-B, B-C, C-D, D-A,
    /// if ABC is in the same module, D is in another module, then there are two interfaces C-D, D-A between modules ABC and D)
    pub interfaces: Vec<Weak<Interface>>,

}

create_ptr_types!(DualModuleParallelUnit, DualModuleParallelUnitPtr, DualModuleParallelUnitWeak);

impl DualModuleParallel {

    /// recommended way to create a new instance, given a customized configuration
    pub fn new_config(initializer: &SolverInitializer, mut config: DualModuleParallelConfig) -> Self {
        let mut thread_pool_builder = rayon::ThreadPoolBuilder::new();
        if config.thread_pool_size != 0 {
            thread_pool_builder = thread_pool_builder.num_threads(config.thread_pool_size);
        }
        let thread_pool = thread_pool_builder.build().expect("creating thread pool failed");
        if config.partitions.len() == 0 {
            config.partitions = vec![VertexRange::new(0, initializer.vertex_num)];
        }
        assert!(config.partitions.len() > 0, "0 partition forbidden");
        let mut units = vec![];
        let partition_info = PartitionInfo::new(&config, initializer);
        let unit_count = config.partitions.len() + config.fusions.len();
        if config.partitions.len() == 1 {  // no partition
            assert!(config.fusions.is_empty(), "should be no `fusions` with only 1 partition");
            let dual_module = DualModuleSerial::new(&initializer);
            let dual_module_ptr = DualModuleSerialPtr::new(dual_module);
            let unit = DualModuleParallelUnitPtr::new_wrapper(dual_module_ptr, &partition_info.units[0]);
            units.push(unit);
        } else {  // multiple partitions, do the initialization in parallel to take advantage of multiple cores
            let complete_graph = CompleteGraph::new(initializer.vertex_num, &initializer.weighted_edges);  // build the graph to construct the NN data structure
            let mut contained_vertices_vec: Vec<BTreeSet<VertexIndex>> = vec![];  // all vertices maintained by each unit
            let mut is_vertex_virtual: Vec<_> = (0..initializer.vertex_num).map(|_| false).collect();
            for virtual_vertex in initializer.virtual_vertices.iter() {
                is_vertex_virtual[*virtual_vertex] = true;
            }
            let mut partitioned_initializers: Vec<PartitionedSolverInitializer> = (0..unit_count).map(|unit_index| {
                let mut interfaces = vec![];
                let mut current_index = unit_index;
                let owning_range = &partition_info.units[unit_index].owning_range;
                let mut contained_vertices = BTreeSet::new();
                for vertex_index in owning_range.iter() {
                    contained_vertices.insert(vertex_index);
                }
                while let Some(parent_index) = &partition_info.units[current_index].parent {
                    let mut mirror_vertices = vec![];
                    if config.edges_in_fusion_unit {
                        for vertex_index in partition_info.units[*parent_index].owning_range.iter() {
                            let mut is_incident = false;
                            for (peer_index, _) in complete_graph.vertices[vertex_index].edges.iter() {
                                if owning_range.contains(peer_index) {
                                    is_incident = true;
                                    break
                                }
                            }
                            if is_incident {
                                mirror_vertices.push((vertex_index, is_vertex_virtual[vertex_index]));
                                contained_vertices.insert(vertex_index);
                            }
                        }
                    } else {
                        // first check if there EXISTS any vertex that's adjacent of it's contains vertex
                        let mut has_incident = false;
                        for vertex_index in partition_info.units[*parent_index].owning_range.iter() {
                            for (peer_index, _) in complete_graph.vertices[vertex_index].edges.iter() {
                                if contained_vertices.contains(peer_index) {  // important diff: as long as it has an edge with contained vertex, add it
                                    has_incident = true;
                                    break
                                }
                            }
                            if has_incident {
                                break
                            }
                        }
                        if has_incident {
                            // add all vertices as mirrored
                            for vertex_index in partition_info.units[*parent_index].owning_range.iter() {
                                mirror_vertices.push((vertex_index, is_vertex_virtual[vertex_index]));
                                contained_vertices.insert(vertex_index);
                            }
                        }
                    }
                    if !mirror_vertices.is_empty() {  // only add non-empty mirrored parents is enough
                        interfaces.push((*parent_index, mirror_vertices));
                    }
                    current_index = *parent_index;
                }
                contained_vertices_vec.push(contained_vertices);
                PartitionedSolverInitializer {
                    vertex_num: initializer.vertex_num,
                    owning_range: owning_range.clone(),
                    weighted_edges: vec![],  // to be filled later
                    interfaces: interfaces,
                    virtual_vertices: owning_range.iter().filter(|vertex_index| is_vertex_virtual[*vertex_index]).collect(),
                }  // note that all fields can be modified later
            }).collect();
            // assign each edge to its unique partition
            for &(i, j, weight) in initializer.weighted_edges.iter() {
                assert_ne!(i, j, "invalid edge from and to the same vertex {}", i);
                assert!(i < initializer.vertex_num, "edge ({}, {}) connected to an invalid vertex {}", i, j, i);
                assert!(j < initializer.vertex_num, "edge ({}, {}) connected to an invalid vertex {}", i, j, j);
                let i_unit_index = partition_info.vertex_to_owning_unit[i];
                let j_unit_index = partition_info.vertex_to_owning_unit[j];
                // either left is ancestor of right or right is ancestor of left, otherwise the edge is invalid (because crossing two independent partitions)
                let is_i_ancestor = partition_info.units[i_unit_index].descendants.contains(&j_unit_index);
                let is_j_ancestor = partition_info.units[j_unit_index].descendants.contains(&i_unit_index);
                assert!(is_i_ancestor || is_j_ancestor || i_unit_index == j_unit_index, "violating edge ({}, {}) crossing two independent partitions {} and {}"
                    , i, j, i_unit_index, j_unit_index);
                let ancestor_unit_index = if is_i_ancestor { i_unit_index } else { j_unit_index };
                let descendant_unit_index = if is_i_ancestor { j_unit_index } else { i_unit_index };
                if config.edges_in_fusion_unit {
                    // the edge should be added to the descendant, and it's guaranteed that the descendant unit contains (although not necessarily owned) the vertex
                    partitioned_initializers[descendant_unit_index].weighted_edges.push((i, j, weight));
                } else {
                    // add edge to every unit from the descendant (including) and the ancestor (excluding) who mirrored the vertex
                    if ancestor_unit_index < config.partitions.len() {
                        // leaf unit holds every unit
                        partitioned_initializers[descendant_unit_index].weighted_edges.push((i, j, weight));
                    } else {
                        // iterate every leaf unit of the `descendant_unit_index` to see if adding the edge or not
                        fn dfs_add(unit_index: usize, config: &DualModuleParallelConfig, partition_info: &PartitionInfo, i: VertexIndex, j: VertexIndex
                                , weight: Weight, contained_vertices_vec: &Vec<BTreeSet<VertexIndex>>, partitioned_initializers: &mut Vec<PartitionedSolverInitializer>) {
                            if unit_index >= config.partitions.len() {
                                let (left_index, right_index) = &partition_info.units[unit_index].children.expect("fusion unit must have children");
                                dfs_add(*left_index, config, partition_info, i, j, weight, contained_vertices_vec, partitioned_initializers);
                                dfs_add(*right_index, config, partition_info, i, j, weight, contained_vertices_vec, partitioned_initializers);
                            } else {
                                let contain_i = contained_vertices_vec[unit_index].contains(&i);
                                let contain_j = contained_vertices_vec[unit_index].contains(&j);
                                assert!(!(contain_i ^ contain_j), "{} and {} must either be both contained or not contained by {}", i, j, unit_index);
                                if contain_i {
                                    partitioned_initializers[unit_index].weighted_edges.push((i, j, weight));
                                }
                            }
                        }
                        dfs_add(descendant_unit_index, &config, &partition_info, i, j, weight, &contained_vertices_vec, &mut partitioned_initializers);
                    }
                }
            }
            println!("partitioned_initializers: {:?}", partitioned_initializers);
            thread_pool.scope(|_| {
                (0..unit_count).into_par_iter().map(|unit_index| {
                    println!("unit_index: {unit_index}");
                    let dual_module = DualModuleSerial::new_partitioned(&partitioned_initializers[unit_index]);
                    let dual_module_ptr = DualModuleSerialPtr::new(dual_module);
                    let unit = DualModuleParallelUnitPtr::new_wrapper(dual_module_ptr, &partition_info.units[unit_index]);
                    unit
                }).collect_into_vec(&mut units);
            });
        }
        Self {
            initializer: initializer.clone(),
            units: units,
            config: config,
            partition_info: partition_info,
            thread_pool: thread_pool,
        }
    }

    /// find the active ancestor to handle this dual node (should be unique, i.e. any time only one ancestor is active)
    pub fn find_active_ancestor(&self, dual_node_ptr: &DualNodePtr) -> DualModuleParallelUnitPtr {
        // find the first active ancestor unit that should handle this dual node
        let representative_vertex = dual_node_ptr.get_representative_vertex();
        let owning_unit_index = self.partition_info.vertex_to_owning_unit[representative_vertex];
        let mut owning_unit_ptr = self.units[owning_unit_index].clone();
        loop {
            let owning_unit = owning_unit_ptr.read_recursive();
            if owning_unit.is_active {
                break  // find an active unit
            }
            let parent_owning_unit_ptr = owning_unit.parent.clone().unwrap().upgrade_force();
            drop(owning_unit);
            owning_unit_ptr = parent_owning_unit_ptr;
        }
        owning_unit_ptr
    }

}

impl DualModuleImpl for DualModuleParallel {

    /// initialize the dual module, which is supposed to be reused for multiple decoding tasks with the same structure
    fn new(initializer: &SolverInitializer) -> Self {
        Self::new_config(initializer, DualModuleParallelConfig::default())
    }

    /// clear all growth and existing dual nodes
    fn clear(&mut self) {
        self.thread_pool.scope(|_| {
            self.units.par_iter().enumerate().for_each(|(unit_idx, unit_ptr)| {
                let mut unit = unit_ptr.write();
                unit.clear();
                unit.is_fused = false;  // everything is not fused at the beginning
                unit.is_active = unit_idx < self.config.partitions.len();  // only partitioned serial modules are active at the beginning
            });
        })
    }

    // although not the intended way to use it, we do support these common APIs for compatibility with normal primal modules

    fn add_dual_node(&mut self, dual_node_ptr: &DualNodePtr) {
        self.thread_pool.scope(|_| {
            let unit_ptr = self.find_active_ancestor(&dual_node_ptr);
            let mut unit = unit_ptr.write();
            unit.add_dual_node(&dual_node_ptr);
        })
    }

    fn remove_blossom(&mut self, dual_node_ptr: DualNodePtr) {
        self.thread_pool.scope(|_| {
            let unit_ptr = self.find_active_ancestor(&dual_node_ptr);
            let mut unit = unit_ptr.write();
            unit.remove_blossom(dual_node_ptr);
        })
    }

    fn set_grow_state(&mut self, dual_node_ptr: &DualNodePtr, grow_state: DualNodeGrowState) {
        self.thread_pool.scope(|_| {
            let unit_ptr = self.find_active_ancestor(&dual_node_ptr);
            let mut unit = unit_ptr.write();
            unit.set_grow_state(&dual_node_ptr, grow_state);
        })
    }

    fn compute_maximum_update_length_dual_node(&mut self, dual_node_ptr: &DualNodePtr, is_grow: bool, simultaneous_update: bool) -> MaxUpdateLength {
        self.thread_pool.scope(|_| {
            let unit_ptr = self.find_active_ancestor(&dual_node_ptr);
            let mut unit = unit_ptr.write();
            unit.compute_maximum_update_length_dual_node(&dual_node_ptr, is_grow, simultaneous_update)
        })
    }

    fn compute_maximum_update_length(&mut self) -> GroupMaxUpdateLength {
        self.thread_pool.scope(|_| {
            let results: Vec<_> = self.units.par_iter().filter_map(|unit_ptr| {
                let mut unit = unit_ptr.write();
                if !unit.is_active { return None }
                Some(unit.compute_maximum_update_length())
            }).collect();
            let mut group_max_update_length = GroupMaxUpdateLength::new();
            for local_group_max_update_length in results.into_iter() {
                group_max_update_length.extend(local_group_max_update_length);
            }
            group_max_update_length
        })
    }

    fn grow_dual_node(&mut self, dual_node_ptr: &DualNodePtr, length: Weight) {
        self.thread_pool.scope(|_| {
            let unit_ptr = self.find_active_ancestor(&dual_node_ptr);
            let mut unit = unit_ptr.write();
            unit.grow_dual_node(&dual_node_ptr, length);
        })
    }

    fn grow(&mut self, length: Weight) {
        self.thread_pool.scope(|_| {
            self.units.par_iter().for_each(|unit_ptr| {
                let mut unit = unit_ptr.write();
                if !unit.is_active { return }
                unit.grow(length);
            });
        })
    }

    fn load_edge_modifier(&mut self, edge_modifier: &Vec<(EdgeIndex, Weight)>) {
        self.thread_pool.scope(|_| {
            self.units.par_iter().for_each(|unit_ptr| {
                let mut unit = unit_ptr.write();
                if !unit.is_active { return }
                unit.load_edge_modifier(edge_modifier);
            });
        })
    }

}


/*
Implementing visualization functions
*/

impl FusionVisualizer for DualModuleParallel {
    fn snapshot(&self, abbrev: bool) -> serde_json::Value {
        // do the sanity check first before taking snapshot
        // self.sanity_check().unwrap();
        let mut value = json!({});
        for unit_ptr in self.units.iter() {
            let unit = unit_ptr.read_recursive();
            let value_2 = unit.snapshot(abbrev);
            snapshot_combine_values(&mut value, value_2, abbrev);
        }
        value
    }
}

impl FusionVisualizer for DualModuleParallelUnit {
    fn snapshot(&self, abbrev: bool) -> serde_json::Value {
        self.serial_module.read_recursive().snapshot(abbrev)
    }
}

impl DualModuleParallelUnitPtr {

    /// create a simple wrapper over a serial dual module
    pub fn new_wrapper(dual_module_ptr: DualModuleSerialPtr, partition_unit_info: &PartitionUnitInfo) -> Self {
        Self::new(DualModuleParallelUnit {
            is_active: true,
            is_fused: false,
            whole_range: partition_unit_info.whole_range,
            owning_range: partition_unit_info.owning_range,
            serial_module: dual_module_ptr,
            children: None,
            parent: None,
            interfaces: vec![],
            nodes: vec![],
        })
    }

}

/// We cannot implement async function because a RwLockWriteGuard implements !Send
impl DualModuleImpl for DualModuleParallelUnit {

    /// clear all growth and existing dual nodes
    fn new(_initializer: &SolverInitializer) -> Self {
        panic!("creating parallel unit directly from initializer is forbidden, use `DualModuleParallel::new` instead");
    }

    /// clear all growth and existing dual nodes
    fn clear(&mut self) {
        self.serial_module.write().clear()
    }

    /// add a new dual node from dual module root
    fn add_dual_node(&mut self, dual_node_ptr: &DualNodePtr) {
        // TODO: determine whether `dual_node_ptr` has anything to do with the underlying dual module, if not, simply return
        self.serial_module.write().add_dual_node(dual_node_ptr)
    }

    fn remove_blossom(&mut self, dual_node_ptr: DualNodePtr) {
        self.serial_module.write().remove_blossom(dual_node_ptr)
    }

    fn set_grow_state(&mut self, dual_node_ptr: &DualNodePtr, grow_state: DualNodeGrowState) {
        self.serial_module.write().set_grow_state(dual_node_ptr, grow_state)
    }

    fn compute_maximum_update_length_dual_node(&mut self, dual_node_ptr: &DualNodePtr, is_grow: bool, simultaneous_update: bool) -> MaxUpdateLength {
        self.serial_module.write().compute_maximum_update_length_dual_node(dual_node_ptr, is_grow, simultaneous_update)
    }

    fn compute_maximum_update_length(&mut self) -> GroupMaxUpdateLength {
        self.serial_module.write().compute_maximum_update_length()
    }

    fn grow_dual_node(&mut self, dual_node_ptr: &DualNodePtr, length: Weight) {
        self.serial_module.write().grow_dual_node(dual_node_ptr, length)
    }

    fn grow(&mut self, length: Weight) {
        self.serial_module.write().grow(length)
    }

    fn load_edge_modifier(&mut self, edge_modifier: &Vec<(EdgeIndex, Weight)>) {
        self.serial_module.write().load_edge_modifier(edge_modifier)
    }

}

/// interface consists of several vertices; each vertex exists as a virtual vertex in several different serial dual modules.
/// each virtual vertex exists in at most one interface
pub struct InterfaceData {
    /// the serial dual modules that processes these virtual vertices,
    pub possession_modules: Vec<DualModuleSerialWeak>,
    /// the virtual vertices references in different modules, [idx of serial dual module] [idx of interfacing vertex]
    pub interfacing_vertices: Vec<Vec<VertexWeak>>,
}

/// interface between dual modules, consisting of a list of nodes of virtual nodes that sits on different modules
pub struct Interface {
    /// unique interface id for ease of zero-cost switching
    pub interface_id: usize,
    /// link to interface data
    pub data: Weak<InterfaceData>,
}


#[cfg(test)]
pub mod tests {
    use super::*;
    use super::super::example::*;
    use super::super::primal_module::*;
    use super::super::primal_module_serial::*;

    pub fn dual_module_parallel_basic_standard_syndrome_optional_viz<F>(d: usize, visualize_filename: Option<String>, syndrome_vertices: Vec<VertexIndex>
            , final_dual: Weight, partition_func: F)
            -> (DualModuleInterface, PrimalModuleSerial, DualModuleParallel) where F: Fn(&SolverInitializer, &mut DualModuleParallelConfig) {
        println!("{syndrome_vertices:?}");
        let half_weight = 500;
        let mut code = CodeCapacityPlanarCode::new(d, 0.1, half_weight);
        let mut visualizer = match visualize_filename.as_ref() {
            Some(visualize_filename) => {
                let mut visualizer = Visualizer::new(Some(visualize_data_folder() + visualize_filename.as_str())).unwrap();
                visualizer.set_positions(code.get_positions(), true);  // automatic center all nodes
                print_visualize_link(&visualize_filename);
                Some(visualizer)
            }, None => None
        };
        // create dual module
        let initializer = code.get_initializer();
        let mut config = DualModuleParallelConfig::default();
        partition_func(&initializer, &mut config);
        let mut dual_module = DualModuleParallel::new_config(&initializer, config);
        // create primal module
        let mut primal_module = PrimalModuleSerial::new(&initializer);
        primal_module.debug_resolve_only_one = true;  // to enable debug mode
        // try to work on a simple syndrome
        code.set_syndrome(&syndrome_vertices);
        let mut interface = DualModuleInterface::new(&code.get_syndrome(), &mut dual_module);
        interface.debug_print_actions = true;
        primal_module.load(&interface);  // load syndrome and connect to the dual module interface
        visualizer.as_mut().map(|v| v.snapshot_combined(format!("syndrome"), vec![&interface, &dual_module, &primal_module]).unwrap());
        // grow until end
        let mut group_max_update_length = dual_module.compute_maximum_update_length();
        while !group_max_update_length.is_empty() {
            println!("group_max_update_length: {:?}", group_max_update_length);
            if let Some(length) = group_max_update_length.get_none_zero_growth() {
                interface.grow(length, &mut dual_module);
                visualizer.as_mut().map(|v| v.snapshot_combined(format!("grow {length}"), vec![&interface, &dual_module, &primal_module]).unwrap());
            } else {
                let first_conflict = format!("{:?}", group_max_update_length.get_conflicts().peek().unwrap());
                primal_module.resolve(group_max_update_length, &mut interface, &mut dual_module);
                visualizer.as_mut().map(|v| v.snapshot_combined(format!("resolve {first_conflict}"), vec![&interface, &dual_module, &primal_module]).unwrap());
            }
            group_max_update_length = dual_module.compute_maximum_update_length();
        }
        assert_eq!(interface.sum_dual_variables, final_dual * 2 * half_weight, "unexpected final dual variable sum");
        (interface, primal_module, dual_module)
    }

    pub fn dual_module_parallel_standard_syndrome<F>(d: usize, visualize_filename: String, syndrome_vertices: Vec<VertexIndex>
            , final_dual: Weight, partition_func: F)
            -> (DualModuleInterface, PrimalModuleSerial, DualModuleParallel) where F: Fn(&SolverInitializer, &mut DualModuleParallelConfig) {
        dual_module_parallel_basic_standard_syndrome_optional_viz(d, Some(visualize_filename), syndrome_vertices, final_dual, partition_func)
    }

    /// test a simple case
    #[test]
    fn dual_module_parallel_basic_1() {  // cargo test dual_module_parallel_basic_1 -- --nocapture
        let visualize_filename = format!("dual_module_parallel_basic_1.json");
        let syndrome_vertices = vec![39, 52, 63, 90, 100];
        dual_module_parallel_standard_syndrome(11, visualize_filename, syndrome_vertices, 9, |initializer, config| {
            println!("initializer: {initializer:?}");
            println!("config: {config:?}");
        });
    }

    /// split into 2, with no syndrome vertex on the interface
    #[test]
    fn dual_module_parallel_basic_2() {  // cargo test dual_module_parallel_basic_2 -- --nocapture
        let visualize_filename = format!("dual_module_parallel_basic_2.json");
        let syndrome_vertices = vec![39, 52, 63, 90, 100];
        dual_module_parallel_standard_syndrome(11, visualize_filename, syndrome_vertices, 9, |_initializer, config| {
            config.partitions = vec![
                VertexRange::new(0, 72),    // unit 0
                VertexRange::new(84, 132),  // unit 1
            ];
            config.fusions = vec![
                (0, 1),  // unit 2, by fusing 0 and 1
            ];
            println!("{config:?}");
        });
    }

}