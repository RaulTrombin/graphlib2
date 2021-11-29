use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt;
use std::hash::{BuildHasherDefault};

use pyo3::create_exception;
use pyo3::exceptions;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3::types::PyTuple;
use pyo3::{Py, PyAny, Python};
use nohash_hasher::{IntMap,IntSet};
use seahash::SeaHasher;

mod hashedany;
use crate::hashedany::HashedAny;

create_exception!(graphlib2, CycleError, exceptions::PyValueError);


type BuildSeaHasher = BuildHasherDefault<SeaHasher>;


fn hashed_node_to_str(node: &HashedAny) -> PyResult<String> {
    Python::with_gil(|py| -> PyResult<String> {
        Ok(node.0.as_ref(py).repr()?.to_str()?.to_string())
    })
}


// Use the result of calling repr() on the Python object as the debug string value
impl fmt::Debug for HashedAny {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", hashed_node_to_str(self).unwrap())
    }
}



#[derive(Debug, Clone, Copy)]
enum NodeState {
    Active,
    Ready,
    Done,
}


#[derive(Clone,Debug)]
struct NodeInfo {
    node: HashedAny,
    state: NodeState,
    npredecessors: u32,
}


#[pyclass(module = "graphlib2",freelist=8)]
#[derive(Clone)]
struct TopologicalSorter {
    id2nodeinfo: IntMap<u32, NodeInfo>,
    node2id: HashMap<HashedAny, u32, BuildSeaHasher>,
    parents: IntMap<u32, HashSet<u32>>,
    children: IntMap<u32, IntSet<u32>>,
    ready_nodes: VecDeque<u32>,
    n_passed_out: u32,
    n_finished: u32,
    prepared: bool,
    iterating: bool,
    node_id_counter: u32,
    node_id_factory: PyObject,
}

impl TopologicalSorter {
    fn mark_node_as_done(
        &mut self,
        node: u32,
        done_queue: Option<&mut VecDeque<u32>>,
    ) -> PyResult<()> {
        // Check that this node is ready to be marked as done and mark it
        // There is currently a remove and an insert here just to take ownership of the value
        // so that we can reference it while modifying other values
        // Maybe there's a better way?
        let nodeinfo = self.id2nodeinfo.get_mut(&node).unwrap();
        match nodeinfo.state {
            NodeState::Active => {
                return Err(exceptions::PyValueError::new_err(format!(
                    "node {} was not passed out (still not ready)",
                    hashed_node_to_str(&nodeinfo.node)?
                )))
            }
            NodeState::Done => {
                return Err(exceptions::PyValueError::new_err(format!(
                    "node {} was already marked as done",
                    hashed_node_to_str(&nodeinfo.node)?
                )))
            }
            NodeState::Ready => nodeinfo.state = NodeState::Done,
        };
        self.n_finished += 1;
        // Find all parents and reduce their dependency count by one,
        // returning all parents w/o any further dependencies
        let q = match done_queue {
            Some(v) => v,
            None => &mut self.ready_nodes,
        };
        let mut parent_info: &mut NodeInfo;
        for parent in self.parents.get(&node).unwrap() {
            parent_info = self.id2nodeinfo.get_mut(&parent).unwrap();
            parent_info.npredecessors -= 1;
            if parent_info.npredecessors == 0 {
                parent_info.state = NodeState::Ready;
                q.push_back(*parent);
            }
        }
        Ok(())
    }
    fn new_node(&mut self, node: &HashedAny) -> u32 {
        let node_id = Python::with_gil(|py| -> u32 {
            u32::extract(self.node_id_factory.call0(py).unwrap().as_ref(py)).unwrap()
        });
        let nodeinfo = NodeInfo {
            node: node.clone(),
            state: NodeState::Active,
            npredecessors: 0,
        };
        self.node2id.insert(node.clone(), node_id);
        self.id2nodeinfo.insert(node_id, nodeinfo);
        self.parents.insert(node_id, HashSet::default());
        self.children.insert(node_id, HashSet::default());
        self.node_id_counter += 1;
        node_id
    }
    fn get_or_insert_node_id(&mut self, node: &HashedAny) -> u32 {
        match self.node2id.get(node) {
            Some(&v) => return v,
            None => (),
        }
        self.new_node(node)
    }
    fn add_node(&mut self, node: HashedAny, children: Vec<HashedAny>) -> PyResult<()> {
        // Insert if it doesn't exist
        let node_id = self.get_or_insert_node_id(&node);
        let mut child_id: u32;
        for child in children {
            child_id = self.get_or_insert_node_id(&child);
            let new_child = self.children.get_mut(&node_id).unwrap().insert(child_id);
            if new_child {
                self.id2nodeinfo.get_mut(&node_id).unwrap().npredecessors += 1;
            }
            self.parents.get_mut(&child_id).unwrap().insert(node_id);
        }
        Ok(())
    }
    fn find_cycle(&self) -> Option<Vec<u32>> {
        let mut seen: HashSet<u32> = HashSet::new();
        let mut stack = Vec::new();
        let mut itstack = Vec::new();
        let mut node2stackid = IntMap::default();
        let mut node: u32;

        for n in self.id2nodeinfo.keys() {
            node = *n;
            // // Only begin exploring from root nodes
            // if nodeinfo.parents.len() != 0 {
            //     continue;
            // }
            if seen.contains(&node) {
                continue;
            }
            'outer: loop {
                if seen.contains(&node) {
                    // If this node is in the current stack, we have a cycle
                    if node2stackid.contains_key(&node) {
                        let start_id = node2stackid.get(&node).unwrap();
                        let mut res = stack[*start_id as usize..].to_vec();
                        res.push(node);
                        return Some(res);
                    }
                } else {
                    seen.insert(node);
                    itstack.push(self.parents.get(&node).unwrap().iter());
                    node2stackid.insert(node, stack.len() as u32);
                    stack.push(node);
                }
                // Backtrack to the topmost stack entry with at least 1 parent
                let mut broke = false;
                while !stack.is_empty() {
                    match itstack.last_mut().unwrap().next() {
                        Some(parent) => {
                            node = *parent;
                            broke = true;
                            break;
                        }
                        None => {
                            node2stackid.remove(&stack.pop().unwrap());
                            itstack.pop();
                            continue;
                        }
                    }
                }
                if !broke {
                    break 'outer;
                }
            }
        }
        None
    }
}

#[pymethods]
impl TopologicalSorter {
    fn add(&mut self, node: HashedAny, predecessors: Vec<HashedAny>) -> PyResult<()> {
        self.add_node(node, predecessors)?;
        Ok(())
    }
    fn prepare(&mut self) -> PyResult<()> {
        if self.prepared {
            return Err(exceptions::PyValueError::new_err(
                "cannot prepare() more than once",
            ));
        }
        match self.find_cycle() {
            Some(cycle) => {
                let maybe_items: PyResult<Vec<String>> = cycle
                    .iter()
                    .map(|n| hashed_node_to_str(&self.id2nodeinfo.get(&n).unwrap().node))
                    .collect();
                let items = maybe_items?;
                let items_str = items.clone().join(", ");
                return Err(CycleError::new_err((
                    format!("nodes are in a cycle [{}]", items_str),
                    items,
                )));
            }
            None => (),
        }
        self.prepared = true;
        for (&node, nodeinfo) in self.id2nodeinfo.iter_mut() {
            if nodeinfo.npredecessors == 0 {
                self.ready_nodes.push_back(node);
                nodeinfo.state = NodeState::Ready;
            }
        }
        Ok(())
    }
    #[new]
    fn new(graph: Option<&PyDict>, node_id_factory: PyObject) -> PyResult<Self> {
        let mut this = TopologicalSorter {
            id2nodeinfo: IntMap::default(),
            node2id: HashMap::default(),
            parents: IntMap::default(),
            children: IntMap::default(),
            ready_nodes: VecDeque::new(),
            n_passed_out: 0,
            n_finished: 0,
            prepared: false,
            iterating: false,
            node_id_counter: 0,
            node_id_factory: node_id_factory,
        };
        if !graph.is_none() {
            for (node, v) in graph.unwrap().iter() {
                let i = v.iter()?;
                let mut children: Vec<HashedAny> = Vec::new();
                for el in i {
                    children.push(HashedAny::extract(el?)?);
                }
                this.add_node(node.extract()?, children)?;
            }
        }
        Ok(this)
    }
    /// Returns string representation of the graph
    fn __str__(&self) -> PyResult<String> {
        Ok(format!("TopologicalSorter()"))
    }
    fn __repr__(&self) -> PyResult<String> {
        self.__str__()
    }
    /// Returns a deep copy of this graph
    fn copy(&self) -> TopologicalSorter {
        self.clone()
    }
    /// Returns any nodes with no dependencies after marking `node` as done
    /// # Arguments
    ///
    /// * `node` - A node in the graph
    fn done(&mut self, nodes: Vec<HashedAny>, py: Python) -> PyResult<()> {
        py.allow_threads(|| 
            {
                if !self.prepared {
                    return Err(exceptions::PyValueError::new_err(
                        "prepare() must be called first",
                    ));
                }
                let mut nodeid: u32;
                for node in nodes {
                    nodeid = match self.node2id.get(&node) {
                        Some(&v) => v,
                        None => {
                            return Err(PyValueError::new_err(format!(
                                "node {} was not added using add()",
                                hashed_node_to_str(&node)?
                            )))
                        }
                    };
                    self.mark_node_as_done(nodeid, None)?;
                }
                Ok(())
            }
        )
    }
    fn is_active(&self) -> PyResult<bool> {
        if !self.prepared {
            return Err(exceptions::PyValueError::new_err(
                "prepare() must be called first",
            ));
        }
        Ok(self.n_finished < self.n_passed_out || !self.ready_nodes.is_empty())
    }
    /// Returns all nodes with no dependencies
    fn get_ready<'py>(&mut self, py: Python<'py>) -> PyResult<&'py PyTuple> {
        let ret = py.allow_threads(|| {
                if !self.prepared {
                    return Err(exceptions::PyValueError::new_err(
                        "prepare() must be called first",
                    ));
                }
                let mut ret: Vec<Py<PyAny>> = Vec::with_capacity(self.ready_nodes.len());
                for node in &self.ready_nodes {
                    ret.push(self.id2nodeinfo.get(&node).unwrap().node.0.clone())
                }
                self.n_passed_out += self.ready_nodes.len() as u32;
                self.ready_nodes.clear();
                Ok(ret)
            }
        )?;
        Ok(PyTuple::new(py,&ret))
    }
    fn static_order<'py>(&mut self) -> PyResult<Vec<Py<PyAny>>> {
        self.prepare()?;
        let mut out = Vec::new();
        let mut queue: VecDeque<u32> = VecDeque::from(self.ready_nodes.clone());
        let mut node: u32;
        loop {
            if queue.is_empty() {
                break;
            }
            node = queue.pop_front().unwrap();
            self.mark_node_as_done(node, Some(&mut queue))?;
            out.push(self.id2nodeinfo.get(&node).unwrap().node.0.clone());
        }
        self.n_passed_out += out.len() as u32;
        self.n_finished += out.len() as u32;
        self.ready_nodes.clear();
        Ok(out)
    }
    fn remove_nodes(&mut self, nodes: Vec<HashedAny>) -> PyResult<()> {
        if !self.prepared {
            return Err(exceptions::PyValueError::new_err(
                "prepare() must be called before remove_nodes()",
            ));
        }
        let mut queue: VecDeque<u32> = VecDeque::with_capacity(nodes.len());
        for node in nodes {
            match self.node2id.get(&node) {
                Some(v) => queue.push_back(*v),
                None => {
                    return Err(
                        PyValueError::new_err(
                            format!(
                                "The node {:?} was not added using add()",
                                node
                            )
                        )
                    )
                }
            }
        }
        let mut node: u32;
        loop {
            if queue.is_empty() {
                break;
            }
            node = queue.pop_front().unwrap();
            match self.id2nodeinfo.remove(&node) {
                Some(_) => (),
                None => continue,  // node was already removed
            }
            for child in self.children.remove(&node).unwrap() {
                queue.push_back(child)
            }
            for parent in self.parents.remove(&node).unwrap() {
                match self.id2nodeinfo.get_mut(&parent) {
                    Some(mut parent_nodeinfo) => {
                        parent_nodeinfo.npredecessors -= 1;
                        self.children.get_mut(&parent).unwrap().remove(&node);
                    }
                    None => (), // parent was removed
                }
            }
        }
        self.ready_nodes.clear();
        for (&node, nodeinfo) in self.id2nodeinfo.iter_mut() {
            if nodeinfo.npredecessors == 0 {
                self.ready_nodes.push_back(node);
                nodeinfo.state = NodeState::Ready;
            }
        }   
        Ok(())
    }
}

#[pymodule]
fn graphlib2(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<TopologicalSorter>()?;
    m.add("CycleError", _py.get_type::<CycleError>())?;
    Ok(())
}
