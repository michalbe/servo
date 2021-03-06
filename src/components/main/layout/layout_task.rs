/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The layout task. Performs layout on the DOM, builds display lists and sends them to be
/// rendered.

use css::matching::MatchMethods;
use css::select::new_stylist;
use css::node_style::StyledNode;
use layout::construct::{FlowConstructionResult, FlowConstructor, NoConstructionResult};
use layout::context::LayoutContext;
use layout::display_list_builder::{DisplayListBuilder, ToGfxColor};
use layout::flow::{Flow, FlowLeafSet, ImmutableFlowUtils, MutableFlowUtils, MutableOwnedFlowUtils};
use layout::flow::{PreorderFlowTraversal, PostorderFlowTraversal};
use layout::flow;
use layout::incremental::RestyleDamage;
use layout::parallel::{AssignHeightsAndStoreOverflowTraversalKind, BubbleWidthsTraversalKind};
use layout::parallel::{UnsafeFlow};
use layout::parallel;
use layout::util::{LayoutDataAccess, OpaqueNode, LayoutDataWrapper};
use layout::wrapper::{DomLeafSet, LayoutNode, TLayoutNode, ThreadSafeLayoutNode};

use extra::arc::{Arc, MutexArc};
use geom::rect::Rect;
use geom::size::Size2D;
use gfx::display_list::{ClipDisplayItemClass, DisplayItem, DisplayItemIterator};
use gfx::display_list::{DisplayList, DisplayListCollection};
use gfx::font_context::FontContextInfo;
use gfx::opts::Opts;
use gfx::render_task::{RenderMsg, RenderChan, RenderLayer};
use gfx::{render_task, color};
use script::dom::event::ReflowEvent;
use script::dom::node::{ElementNodeTypeId, LayoutDataRef};
use script::dom::element::{HTMLBodyElementTypeId, HTMLHtmlElementTypeId};
use script::layout_interface::{AddStylesheetMsg, ContentBoxQuery};
use script::layout_interface::{ContentBoxesQuery, ContentBoxesResponse, ExitNowMsg, LayoutQuery};
use script::layout_interface::{HitTestQuery, ContentBoxResponse, HitTestResponse};
use script::layout_interface::{ContentChangedDocumentDamage, LayoutChan, Msg, PrepareToExitMsg};
use script::layout_interface::{QueryMsg, ReapLayoutDataMsg, Reflow, ReflowDocumentDamage};
use script::layout_interface::{ReflowForDisplay, ReflowMsg};
use script::script_task::{ReflowCompleteMsg, ScriptChan, SendEventMsg};
use servo_msg::constellation_msg::{ConstellationChan, PipelineId};
use servo_net::image_cache_task::{ImageCacheTask, ImageResponseMsg};
use servo_net::local_image_cache::{ImageResponder, LocalImageCache};
use servo_util::geometry::Au;
use servo_util::time::{ProfilerChan, profile};
use servo_util::time;
use servo_util::task::spawn_named;
use servo_util::workqueue::WorkQueue;
use std::cast::transmute;
use std::cast;
use std::cell::RefCell;
use std::comm::Port;
use std::ptr;
use std::util;
use style::{AuthorOrigin, Stylesheet, Stylist};

/// Information needed by the layout task.
pub struct LayoutTask {
    /// The ID of the pipeline that we belong to.
    id: PipelineId,

    /// The port on which we receive messages.
    port: Port<Msg>,

    //// The channel to send messages to ourself.
    chan: LayoutChan,

    /// The channel on which messages can be sent to the constellation.
    constellation_chan: ConstellationChan,

    /// The channel on which messages can be sent to the script task.
    script_chan: ScriptChan,

    /// The channel on which messages can be sent to the painting task.
    render_chan: RenderChan<OpaqueNode>,

    /// The channel on which messages can be sent to the image cache.
    image_cache_task: ImageCacheTask,

    /// The local image cache.
    local_image_cache: MutexArc<LocalImageCache>,

    /// The set of leaves in the DOM tree.
    dom_leaf_set: Arc<DomLeafSet>,

    /// The set of leaves in the flow tree.
    flow_leaf_set: Arc<FlowLeafSet>,

    /// The size of the viewport.
    screen_size: Size2D<Au>,

    /// A cached display list.
    display_list_collection: Option<Arc<DisplayListCollection<OpaqueNode>>>,

    stylist: ~Stylist,

    /// The workers that we use for parallel operation.
    parallel_traversal: Option<WorkQueue<*mut LayoutContext,UnsafeFlow>>,

    /// The channel on which messages can be sent to the profiler.
    profiler_chan: ProfilerChan,

    opts: Opts
}

/// The damage computation traversal.
#[deriving(Clone)]
struct ComputeDamageTraversal;

impl PostorderFlowTraversal for ComputeDamageTraversal {
    #[inline]
    fn process(&mut self, flow: &mut Flow) -> bool {
        let mut damage = flow::base(flow).restyle_damage;
        for child in flow::child_iter(flow) {
            damage.union_in_place(flow::base(*child).restyle_damage.propagate_up())
        }
        flow::mut_base(flow).restyle_damage = damage;
        true
    }
}

/// Propagates restyle damage up and down the tree as appropriate.
///
/// FIXME(pcwalton): Merge this with flow tree building and/or other traversals.
struct PropagateDamageTraversal {
    all_style_damage: bool,
}

impl PreorderFlowTraversal for PropagateDamageTraversal {
    #[inline]
    fn process(&mut self, flow: &mut Flow) -> bool {
        if self.all_style_damage {
            flow::mut_base(flow).restyle_damage.union_in_place(RestyleDamage::all())
        }
        debug!("restyle damage = {:?}", flow::base(flow).restyle_damage);

        let prop = flow::base(flow).restyle_damage.propagate_down();
        if prop.is_nonempty() {
            for kid_ctx in flow::child_iter(flow) {
                flow::mut_base(*kid_ctx).restyle_damage.union_in_place(prop)
            }
        }
        true
    }
}

/// The flow tree verification traversal. This is only on in debug builds.
#[cfg(debug)]
struct FlowTreeVerificationTraversal;

#[cfg(debug)]
impl PreorderFlowTraversal for FlowTreeVerificationTraversal {
    #[inline]
    fn process(&mut self, flow: &mut Flow) -> bool {
        let base = flow::base(flow);
        if !base.flags_info.flags.is_leaf() && !base.flags_info.flags.is_nonleaf() {
            println("flow tree verification failed: flow wasn't a leaf or a nonleaf!");
            flow.dump();
            fail!("flow tree verification failed")
        }
        true
    }
}

/// The bubble-widths traversal, the first part of layout computation. This computes preferred
/// and intrinsic widths and bubbles them up the tree.
pub struct BubbleWidthsTraversal<'a> {
    layout_context: &'a mut LayoutContext,
}

impl<'a> PostorderFlowTraversal for BubbleWidthsTraversal<'a> {
    #[inline]
    fn process(&mut self, flow: &mut Flow) -> bool {
        flow.bubble_widths(self.layout_context);
        true
    }

    // FIXME: We can't prune until we start reusing flows
    /*
    #[inline]
    fn should_prune(&mut self, flow: &mut Flow) -> bool {
        flow::mut_base(flow).restyle_damage.lacks(BubbleWidths)
    }
    */
}

/// The assign-widths traversal. In Gecko this corresponds to `Reflow`.
struct AssignWidthsTraversal<'a>(&'a mut LayoutContext);

impl<'a> PreorderFlowTraversal for AssignWidthsTraversal<'a> {
    #[inline]
    fn process(&mut self, flow: &mut Flow) -> bool {
        flow.assign_widths(**self);
        true
    }
}

/// The assign-heights-and-store-overflow traversal, the last (and most expensive) part of layout
/// computation. Determines the final heights for all layout objects, computes positions, and
/// computes overflow regions. In Gecko this corresponds to `FinishAndStoreOverflow`.
pub struct AssignHeightsAndStoreOverflowTraversal<'a> {
    layout_context: &'a mut LayoutContext,
}

impl<'a> PostorderFlowTraversal for AssignHeightsAndStoreOverflowTraversal<'a> {
    #[inline]
    fn process(&mut self, flow: &mut Flow) -> bool {
        flow.assign_height(self.layout_context);
        flow.store_overflow(self.layout_context);
        true
    }

    #[inline]
    fn should_process(&mut self, flow: &mut Flow) -> bool {
        !flow::base(flow).flags_info.flags.inorder()
    }
}

struct LayoutImageResponder {
    id: PipelineId,
    script_chan: ScriptChan,
}

impl ImageResponder for LayoutImageResponder {
    fn respond(&self) -> proc(ImageResponseMsg) {
        let id = self.id.clone();
        let script_chan = self.script_chan.clone();
        let f: proc(ImageResponseMsg) = proc(_) {
            script_chan.send(SendEventMsg(id.clone(), ReflowEvent))
        };
        f
    }
}

impl LayoutTask {
    /// Spawns a new layout task.
    pub fn create(id: PipelineId,
                  port: Port<Msg>,
                  chan: LayoutChan,
                  constellation_chan: ConstellationChan,
                  script_chan: ScriptChan,
                  render_chan: RenderChan<OpaqueNode>,
                  img_cache_task: ImageCacheTask,
                  opts: Opts,
                  profiler_chan: ProfilerChan,
                  shutdown_chan: Chan<()>) {
        spawn_named("LayoutTask", proc() {
            { // Ensures layout task is destroyed before we send shutdown message
                let mut layout = LayoutTask::new(id,
                                                 port,
                                                 chan,
                                                 constellation_chan,
                                                 script_chan,
                                                 render_chan,
                                                 img_cache_task,
                                                 &opts,
                                                 profiler_chan);
                layout.start();
            }
            shutdown_chan.send(());
        });
    }

    /// Creates a new `LayoutTask` structure.
    fn new(id: PipelineId,
           port: Port<Msg>,
           chan: LayoutChan,
           constellation_chan: ConstellationChan,
           script_chan: ScriptChan,
           render_chan: RenderChan<OpaqueNode>, 
           image_cache_task: ImageCacheTask,
           opts: &Opts,
           profiler_chan: ProfilerChan)
           -> LayoutTask {
        let local_image_cache = MutexArc::new(LocalImageCache(image_cache_task.clone()));
        let screen_size = Size2D(Au(0), Au(0));
        let parallel_traversal = if opts.layout_threads != 1 {
            Some(WorkQueue::new(opts.layout_threads, ptr::mut_null()))
        } else {
            None
        };

        LayoutTask {
            id: id,
            port: port,
            chan: chan,
            constellation_chan: constellation_chan,
            script_chan: script_chan,
            render_chan: render_chan,
            image_cache_task: image_cache_task.clone(),
            local_image_cache: local_image_cache,
            screen_size: screen_size,
            dom_leaf_set: Arc::new(DomLeafSet::new()),
            flow_leaf_set: Arc::new(FlowLeafSet::new()),

            display_list_collection: None,
            stylist: ~new_stylist(),
            parallel_traversal: parallel_traversal,
            profiler_chan: profiler_chan,
            opts: opts.clone()
        }
    }

    /// Starts listening on the port.
    fn start(&mut self) {
        while self.handle_request() {
            // Loop indefinitely.
        }
    }

    // Create a layout context for use in building display lists, hit testing, &c.
    fn build_layout_context(&self, reflow_root: &LayoutNode) -> LayoutContext {
        let font_context_info = FontContextInfo {
            backend: self.opts.render_backend,
            needs_font_list: true,
            profiler_chan: self.profiler_chan.clone(),
        };

        LayoutContext {
            image_cache: self.local_image_cache.clone(),
            screen_size: self.screen_size.clone(),
            constellation_chan: self.constellation_chan.clone(),
            dom_leaf_set: self.dom_leaf_set.clone(),
            flow_leaf_set: self.flow_leaf_set.clone(),
            layout_chan: self.chan.clone(),
            font_context_info: font_context_info,
            stylist: &*self.stylist,
            reflow_root: OpaqueNode::from_layout_node(reflow_root),
        }
    }

    /// Receives and dispatches messages from the port.
    fn handle_request(&mut self) -> bool {
        match self.port.recv() {
            AddStylesheetMsg(sheet) => self.handle_add_stylesheet(sheet),
            ReflowMsg(data) => {
                profile(time::LayoutPerformCategory, self.profiler_chan.clone(), || {
                    self.handle_reflow(data);
                });
            }
            QueryMsg(query) => {
                let mut query = Some(query);
                profile(time::LayoutQueryCategory, self.profiler_chan.clone(), || {
                    self.handle_query(query.take_unwrap());
                });
            }
            ReapLayoutDataMsg(dead_layout_data) => {
                unsafe {
                    self.handle_reap_layout_data(dead_layout_data)
                }
            }
            PrepareToExitMsg(response_chan) => {
                debug!("layout: PrepareToExitMsg received");
                self.prepare_to_exit(response_chan);
                return false
            }
            ExitNowMsg => {
                debug!("layout: ExitNowMsg received");
                self.exit_now();
                return false
            }
        }

        true
    }

    /// Enters a quiescent state in which no new messages except for `ReapLayoutDataMsg` will be
    /// processed until an `ExitNowMsg` is received. A pong is immediately sent on the given
    /// response channel.
    fn prepare_to_exit(&mut self, response_chan: Chan<()>) {
        response_chan.send(());
        loop {
            match self.port.recv() {
                ReapLayoutDataMsg(dead_layout_data) => {
                    unsafe {
                        self.handle_reap_layout_data(dead_layout_data)
                    }
                }
                ExitNowMsg => {
                    self.exit_now();
                    break
                }
                _ => {
                    fail!("layout: message that wasn't `ExitNowMsg` received after \
                           `PrepareToExitMsg`")
                }
            }
        }
    }

    /// Shuts down the layout task now. If there are any DOM nodes left, layout will now (safely)
    /// crash.
    fn exit_now(&mut self) {
        let (response_port, response_chan) = Chan::new();
        
        match self.parallel_traversal {
            None => {}
            Some(ref mut traversal) => traversal.shutdown(),
        }

        self.render_chan.send(render_task::ExitMsg(response_chan));
        response_port.recv()
    }

    fn handle_add_stylesheet(&mut self, sheet: Stylesheet) {
        self.stylist.add_stylesheet(sheet, AuthorOrigin)
    }

    /// Builds the flow tree.
    ///
    /// This corresponds to the various `nsCSSFrameConstructor` methods in Gecko or
    /// `createRendererIfNeeded` in WebKit. Note, however that in WebKit `createRendererIfNeeded`
    /// is intertwined with selector matching, making it difficult to compare directly. It is
    /// marked `#[inline(never)]` to aid benchmarking in sampling profilers.
    #[inline(never)]
    fn construct_flow_tree(&self, layout_context: &mut LayoutContext, node: LayoutNode) -> ~Flow {
        let node = ThreadSafeLayoutNode::new(node);
        node.traverse_postorder_mut(&mut FlowConstructor::init(layout_context));

        let mut layout_data_ref = node.mutate_layout_data();
        let result = match *layout_data_ref.get() {
            Some(ref mut layout_data) => {
                util::replace(&mut layout_data.data.flow_construction_result, NoConstructionResult)
            }
            None => fail!("no layout data for root node"),
        };
        let mut flow = match result {
            FlowConstructionResult(flow) => flow,
            _ => fail!("Flow construction didn't result in a flow at the root of the tree!"),
        };
        flow.mark_as_root();
        flow
    }

    /// Performs layout constraint solving.
    ///
    /// This corresponds to `Reflow()` in Gecko and `layout()` in WebKit/Blink and should be
    /// benchmarked against those two. It is marked `#[inline(never)]` to aid profiling.
    #[inline(never)]
    fn solve_constraints(&mut self,
                         layout_root: &mut Flow,
                         layout_context: &mut LayoutContext) {
        {
            let mut traversal = BubbleWidthsTraversal {
                layout_context: layout_context,
            };
            layout_root.traverse_postorder(&mut traversal);
        }

        // FIXME(kmc): We want to prune nodes without the Reflow restyle damage
        // bit, but FloatContext values can't be reused, so we need to
        // recompute them every time.
        // NOTE: this currently computes borders, so any pruning should separate that operation
        // out.
        layout_root.traverse_preorder(&mut AssignWidthsTraversal(layout_context));

        // FIXME(pcwalton): Prune this pass as well.
        {
            let mut traversal = AssignHeightsAndStoreOverflowTraversal {
                layout_context: layout_context,
            };
            layout_root.traverse_postorder(&mut traversal);
        }
    }

    /// Performs layout constraint solving in parallel.
    ///
    /// This corresponds to `Reflow()` in Gecko and `layout()` in WebKit/Blink and should be
    /// benchmarked against those two. It is marked `#[inline(never)]` to aid profiling.
    #[inline(never)]
    fn solve_constraints_parallel(&mut self,
                                  layout_root: &mut Flow,
                                  layout_context: &mut LayoutContext) {
        match self.parallel_traversal {
            None => fail!("solve_contraints_parallel() called with no parallel traversal ready"),
            Some(ref mut traversal) => {
                parallel::traverse_flow_tree(BubbleWidthsTraversalKind,
                                             &self.flow_leaf_set,
                                             self.profiler_chan.clone(),
                                             layout_context,
                                             traversal);

                // NOTE: this currently computes borders, so any pruning should separate that
                // operation out.
                // TODO(pcwalton): Run this in parallel as well. This will require a bit more work
                // because this is a top-down traversal, unlike the others.
                layout_root.traverse_preorder(&mut AssignWidthsTraversal(layout_context));

                parallel::traverse_flow_tree(AssignHeightsAndStoreOverflowTraversalKind,
                                             &self.flow_leaf_set,
                                             self.profiler_chan.clone(),
                                             layout_context,
                                             traversal);
            }
        }
    }

    /// Verifies that every node was either marked as a leaf or as a nonleaf in the flow tree.
    /// This is only on in debug builds.
    #[inline(never)]
    #[cfg(debug)]
    fn verify_flow_tree(&mut self, layout_root: &mut ~Flow) {
        let mut traversal = FlowTreeVerificationTraversal;
        layout_root.traverse_preorder(&mut traversal);
    }

    #[cfg(not(debug))]
    fn verify_flow_tree(&mut self, _: &mut ~Flow) {
    }

    /// The high-level routine that performs layout tasks.
    fn handle_reflow(&mut self, data: &Reflow) {
        // FIXME: Isolate this transmutation into a "bridge" module.
        let node: &LayoutNode = unsafe {
            transmute(&data.document_root)
        };

        debug!("layout: received layout request for: {:s}", data.url.to_str());
        debug!("layout: damage is {:?}", data.damage);
        debug!("layout: parsed Node tree");
        debug!("{:?}", node.dump());

        // Reset the image cache.
        unsafe {
            self.local_image_cache.unsafe_access(|local_image_cache| {
                local_image_cache.next_round(self.make_on_image_available_cb())
            });
        }

        // true => Do the reflow with full style damage, because content
        // changed or the window was resized.
        let mut all_style_damage = match data.damage.level {
            ContentChangedDocumentDamage => true,
            _ => false
        };

        let current_screen_size = Size2D(Au::from_px(data.window_size.width as int),
                                         Au::from_px(data.window_size.height as int));
        if self.screen_size != current_screen_size {
            all_style_damage = true
        }
        self.screen_size = current_screen_size;

        // Create a layout context for use throughout the following passes.
        let mut layout_ctx = self.build_layout_context(node);

        let mut layout_root = profile(time::LayoutStyleRecalcCategory,
                                      self.profiler_chan.clone(),
                                      || {
            // Perform CSS selector matching if necessary.
            match data.damage.level {
                ReflowDocumentDamage => {}
                _ => {
                    profile(time::LayoutSelectorMatchCategory, self.profiler_chan.clone(), || {
                        match self.parallel_traversal {
                            None => {
                                node.match_and_cascade_subtree(self.stylist,
                                                               &layout_ctx.layout_chan,
                                                               None)
                            }
                            Some(ref mut traversal) => {
                                parallel::match_and_cascade_subtree(node,
                                                                    &mut layout_ctx,
                                                                    traversal)
                            }
                        }
                    })
                }
            }

            // Construct the flow tree.
            profile(time::LayoutTreeBuilderCategory,
                    self.profiler_chan.clone(),
                    || self.construct_flow_tree(&mut layout_ctx, *node))
        });

        // Verification of the flow tree, which ensures that all nodes were either marked as leaves
        // or as non-leaves. This becomes a no-op in release builds. (It is inconsequential to
        // memory safety but is a useful debugging tool.)
        self.verify_flow_tree(&mut layout_root);

        // Propagate damage.
        profile(time::LayoutDamagePropagateCategory, self.profiler_chan.clone(), || {
            layout_root.traverse_preorder(&mut PropagateDamageTraversal {
                all_style_damage: all_style_damage
            });
            layout_root.traverse_postorder(&mut ComputeDamageTraversal.clone());
        });

        // Perform the primary layout passes over the flow tree to compute the locations of all
        // the boxes.
        profile(time::LayoutMainCategory, self.profiler_chan.clone(), || {
            match self.parallel_traversal {
                None => {
                    // Sequential mode.
                    self.solve_constraints(layout_root, &mut layout_ctx)
                }
                Some(_) => {
                    // Parallel mode.
                    self.solve_constraints_parallel(layout_root, &mut layout_ctx)
                }
            }
        });

        // Build the display list if necessary, and send it to the renderer.
        if data.goal == ReflowForDisplay {
            profile(time::LayoutDispListBuildCategory, self.profiler_chan.clone(), || {
                let root_size = flow::base(layout_root).position.size;
                let mut display_list_collection = DisplayListCollection::new();
                display_list_collection.add_list(DisplayList::<OpaqueNode>::new());
                let display_list_collection = ~RefCell::new(display_list_collection);
                let dirty = flow::base(layout_root).position.clone();
                let display_list_builder = DisplayListBuilder {
                    ctx: &layout_ctx,
                };
                layout_root.build_display_lists(&display_list_builder, &dirty, 0u, display_list_collection);

                let display_list_collection = Arc::new(display_list_collection.unwrap());

                let mut color = color::rgba(255.0, 255.0, 255.0, 255.0);

                for child in node.traverse_preorder() {
                    if child.type_id() == ElementNodeTypeId(HTMLHtmlElementTypeId) || 
                            child.type_id() == ElementNodeTypeId(HTMLBodyElementTypeId) {
                        let element_bg_color = {
                            let thread_safe_child = ThreadSafeLayoutNode::new(child);
                            thread_safe_child.style()
                                             .get()
                                             .resolve_color(thread_safe_child.style()
                                                                             .get()
                                                                             .Background
                                                                             .background_color)
                                             .to_gfx_color()
                        };
                        match element_bg_color {
                            color::rgba(0., 0., 0., 0.) => {}
                            _ => {
                                color = element_bg_color;
                                break;
                           }
                        }
                    }
                }

                let render_layer = RenderLayer {
                    display_list_collection: display_list_collection.clone(),
                    size: Size2D(root_size.width.to_nearest_px() as uint,
                                 root_size.height.to_nearest_px() as uint),
                    color: color
                };

                self.display_list_collection = Some(display_list_collection.clone());

                debug!("Layout done!");

                self.render_chan.send(RenderMsg(render_layer));
            });
        }

        layout_root.destroy(self.flow_leaf_set.get());

        // Tell script that we're done.
        //
        // FIXME(pcwalton): This should probably be *one* channel, but we can't fix this without
        // either select or a filtered recv() that only looks for messages of a given type.
        data.script_join_chan.send(());
        data.script_chan.send(ReflowCompleteMsg(self.id, data.id));
    }

    /// Handles a query from the script task. This is the main routine that DOM functions like
    /// `getClientRects()` or `getBoundingClientRect()` ultimately invoke.
    fn handle_query(&self, query: LayoutQuery) {
        match query {
            // The neat thing here is that in order to answer the following two queries we only
            // need to compare nodes for equality. Thus we can safely work only with `OpaqueNode`.
            ContentBoxQuery(node, reply_chan) => {
                let node = OpaqueNode::from_script_node(&node);

                fn union_boxes_for_node<'a>(
                                        accumulator: &mut Option<Rect<Au>>,
                                        mut iter: DisplayItemIterator<'a,OpaqueNode>,
                                        node: OpaqueNode) {
                    for item in iter {
                        union_boxes_for_node(accumulator, item.children(), node);
                        if item.base().extra == node {
                            match *accumulator {
                                None => *accumulator = Some(item.base().bounds),
                                Some(ref mut acc) => *acc = acc.union(&item.base().bounds),
                            }
                        }
                    }
                }

                let mut rect = None;
                for display_list in self.display_list_collection.as_ref().unwrap().get().iter() {
                    union_boxes_for_node(&mut rect, display_list.iter(), node);
                }
                reply_chan.send(ContentBoxResponse(rect.unwrap_or(Au::zero_rect())))
            }
            ContentBoxesQuery(node, reply_chan) => {
                let node = OpaqueNode::from_script_node(&node);

                fn add_boxes_for_node<'a>(
                                      accumulator: &mut ~[Rect<Au>],
                                      mut iter: DisplayItemIterator<'a,OpaqueNode>,
                                      node: OpaqueNode) {
                    for item in iter {
                        add_boxes_for_node(accumulator, item.children(), node);
                        if item.base().extra == node {
                            accumulator.push(item.base().bounds)
                        }
                    }
                }

                let mut boxes = ~[];
                for display_list in self.display_list_collection.as_ref().unwrap().get().iter() {
                    add_boxes_for_node(&mut boxes, display_list.iter(), node);
                }
                reply_chan.send(ContentBoxesResponse(boxes))
            }
            HitTestQuery(_, point, reply_chan) => {
                fn hit_test(x: Au, y: Au, list: &[DisplayItem<OpaqueNode>])
                            -> Option<HitTestResponse> {
                    for item in list.rev_iter() {
                        match *item {
                            ClipDisplayItemClass(ref cc) => {
                                let ret = hit_test(x, y, cc.child_list);
                                if !ret.is_none() {
                                    return ret;
                                }
                            }
                            _ => {}
                        }
                    }

                    for item in list.rev_iter() {
                        match *item {
                            ClipDisplayItemClass(_) => continue,
                            _ => {}
                        }
                        let bounds = item.bounds();

                        // TODO(tikue): This check should really be performed by a method of
                        // DisplayItem.
                        if x < bounds.origin.x + bounds.size.width &&
                                bounds.origin.x <= x &&
                                y < bounds.origin.y + bounds.size.height &&
                                bounds.origin.y <= y {
                            return Some(HitTestResponse(item.base()
                                                            .extra
                                                            .to_untrusted_node_address()))
                        }
                    }

                    let ret: Option<HitTestResponse> = None;
                    ret
                }
                for display_list in self.display_list_collection.as_ref().unwrap().get().lists.rev_iter() {
                    let (x, y) = (Au::from_frac_px(point.x as f64),
                                  Au::from_frac_px(point.y as f64));
                    let resp = hit_test(x,y,display_list.list);
                    if resp.is_some() {
                        reply_chan.send(Ok(resp.unwrap())); 
                        return
                    }
                }
                reply_chan.send(Err(()));

            }
        }
    }

    // When images can't be loaded in time to display they trigger
    // this callback in some task somewhere. This will send a message
    // to the script task, and ultimately cause the image to be
    // re-requested. We probably don't need to go all the way back to
    // the script task for this.
    fn make_on_image_available_cb(&self) -> ~ImageResponder:Send {
        // This has a crazy signature because the image cache needs to
        // make multiple copies of the callback, and the dom event
        // channel is not a copyable type, so this is actually a
        // little factory to produce callbacks
        ~LayoutImageResponder {
            id: self.id.clone(),
            script_chan: self.script_chan.clone(),
        } as ~ImageResponder:Send
    }

    /// Handles a message to destroy layout data. Layout data must be destroyed on *this* task
    /// because it contains local managed pointers.
    unsafe fn handle_reap_layout_data(&self, layout_data: LayoutDataRef) {
        let mut layout_data_ref = layout_data.borrow_mut();
        let _: Option<LayoutDataWrapper> = cast::transmute(
            util::replace(layout_data_ref.get(), None));
    }
}

