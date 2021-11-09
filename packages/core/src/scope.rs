use crate::innerlude::*;
use fxhash::FxHashMap;
use std::{
    any::{Any, TypeId},
    cell::{Cell, RefCell},
    collections::HashMap,
    future::Future,
    pin::Pin,
    rc::Rc,
};

/// Every component in Dioxus is represented by a `Scope`.
///
/// Scopes contain the state for hooks, the component's props, and other lifecycle information.
///
/// Scopes are allocated in a generational arena. As components are mounted/unmounted, they will replace slots of dead components.
/// The actual contents of the hooks, though, will be allocated with the standard allocator. These should not allocate as frequently.
///
/// We expose the `Scope` type so downstream users can traverse the Dioxus VirtualDOM for whatever
/// use case they might have.
pub struct ScopeInner {
    // Book-keeping about our spot in the arena
    pub(crate) parent_idx: Option<ScopeId>,
    pub(crate) our_arena_idx: ScopeId,
    pub(crate) height: u32,
    pub(crate) subtree: Cell<u32>,
    pub(crate) is_subtree_root: Cell<bool>,

    // Nodes
    pub(crate) frames: ActiveFrame,
    pub(crate) caller: *const dyn for<'b> Fn(&'b ScopeInner) -> Element<'b>,

    /*
    we care about:
    - listeners (and how to call them when an event is triggered)
    - borrowed props (and how to drop them when the parent is dropped)
    - suspended nodes (and how to call their callback when their associated tasks are complete)
    */
    pub(crate) listeners: RefCell<Vec<*const Listener<'static>>>,
    pub(crate) borrowed_props: RefCell<Vec<*const VComponent<'static>>>,
    pub(crate) suspended_nodes: RefCell<FxHashMap<u64, *const VSuspended<'static>>>,

    // State
    pub(crate) hooks: HookList,

    // todo: move this into a centralized place - is more memory efficient
    pub(crate) shared_contexts: RefCell<HashMap<TypeId, Rc<dyn Any>>>,

    // whenever set_state is called, we fire off a message to the scheduler
    // this closure _is_ the method called by schedule_update that marks this component as dirty
    pub(crate) memoized_updater: Rc<dyn Fn()>,

    pub(crate) shared: EventChannel,
}

/// Public interface for Scopes.
impl ScopeInner {
    /// Get the root VNode for this Scope.
    ///
    /// This VNode is the "entrypoint" VNode. If the component renders multiple nodes, then this VNode will be a fragment.
    ///
    /// # Example
    /// ```rust
    /// let mut dom = VirtualDom::new(|(cx, props)|cx.render(rsx!{ div {} }));
    /// dom.rebuild();
    ///
    /// let base = dom.base_scope();
    ///
    /// if let VNode::VElement(node) = base.root_node() {
    ///     assert_eq!(node.tag_name, "div");
    /// }
    /// ```
    pub fn root_node(&self) -> &VNode {
        self.frames.fin_head()
    }

    /// Get the subtree ID that this scope belongs to.
    ///
    /// Each component has its own subtree ID - the root subtree has an ID of 0. This ID is used by the renderer to route
    /// the mutations to the correct window/portal/subtree.
    ///
    ///
    /// # Example
    ///
    /// ```rust
    /// let mut dom = VirtualDom::new(|(cx, props)|cx.render(rsx!{ div {} }));
    /// dom.rebuild();
    ///
    /// let base = dom.base_scope();
    ///
    /// assert_eq!(base.subtree(), 0);
    /// ```
    pub fn subtree(&self) -> u32 {
        self.subtree.get()
    }

    pub(crate) fn new_subtree(&self) -> Option<u32> {
        if self.is_subtree_root.get() {
            None
        } else {
            let cur = self.shared.cur_subtree.get();
            self.shared.cur_subtree.set(cur + 1);
            Some(cur)
        }
    }

    /// Get the height of this Scope - IE the number of scopes above it.
    ///
    /// A Scope with a height of `0` is the root scope - there are no other scopes above it.
    ///
    /// # Example
    ///
    /// ```rust
    /// let mut dom = VirtualDom::new(|(cx, props)|cx.render(rsx!{ div {} }));
    /// dom.rebuild();
    ///
    /// let base = dom.base_scope();
    ///
    /// assert_eq!(base.height(), 0);
    /// ```
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Get the Parent of this Scope within this Dioxus VirtualDOM.
    ///
    /// This ID is not unique across Dioxus VirtualDOMs or across time. IDs will be reused when components are unmounted.
    ///
    /// The base component will not have a parent, and will return `None`.
    ///
    /// # Example
    ///
    /// ```rust
    /// let mut dom = VirtualDom::new(|(cx, props)|cx.render(rsx!{ div {} }));
    /// dom.rebuild();
    ///
    /// let base = dom.base_scope();
    ///
    /// assert_eq!(base.parent(), None);
    /// ```
    pub fn parent(&self) -> Option<ScopeId> {
        self.parent_idx
    }

    /// Get the ID of this Scope within this Dioxus VirtualDOM.
    ///
    /// This ID is not unique across Dioxus VirtualDOMs or across time. IDs will be reused when components are unmounted.
    ///
    /// # Example
    ///
    /// ```rust
    /// let mut dom = VirtualDom::new(|(cx, props)|cx.render(rsx!{ div {} }));
    /// dom.rebuild();
    /// let base = dom.base_scope();
    ///
    /// assert_eq!(base.scope_id(), 0);
    /// ```
    pub fn scope_id(&self) -> ScopeId {
        self.our_arena_idx
    }
}

// The type of closure that wraps calling components
/// The type of task that gets sent to the task scheduler
/// Submitting a fiber task returns a handle to that task, which can be used to wake up suspended nodes
pub type FiberTask = Pin<Box<dyn Future<Output = ScopeId>>>;

/// Private interface for Scopes.
impl ScopeInner {
    // we are being created in the scope of an existing component (where the creator_node lifetime comes into play)
    // we are going to break this lifetime by force in order to save it on ourselves.
    // To make sure that the lifetime isn't truly broken, we receive a Weak RC so we can't keep it around after the parent dies.
    // This should never happen, but is a good check to keep around
    //
    // Scopes cannot be made anywhere else except for this file
    // Therefore, their lifetimes are connected exclusively to the virtual dom
    pub(crate) fn new(
        caller: &dyn for<'b> Fn(&'b ScopeInner) -> Element<'b>,
        our_arena_idx: ScopeId,
        parent_idx: Option<ScopeId>,
        height: u32,
        subtree: u32,
        shared: EventChannel,
    ) -> Self {
        let schedule_any_update = shared.schedule_any_immediate.clone();

        let memoized_updater = Rc::new(move || schedule_any_update(our_arena_idx));

        let caller = caller as *const _;

        // wipe away the associated lifetime - we are going to manually manage the one-way lifetime graph
        let caller = unsafe { std::mem::transmute(caller) };

        Self {
            memoized_updater,
            shared,
            caller,
            parent_idx,
            our_arena_idx,
            height,
            subtree: Cell::new(subtree),
            is_subtree_root: Cell::new(false),

            frames: ActiveFrame::new(),
            hooks: Default::default(),
            suspended_nodes: Default::default(),
            shared_contexts: Default::default(),
            listeners: Default::default(),
            borrowed_props: Default::default(),
        }
    }

    pub(crate) fn update_scope_dependencies(
        &mut self,
        caller: &dyn for<'b> Fn(&'b ScopeInner) -> Element<'b>,
    ) {
        log::debug!("Updating scope dependencies {:?}", self.our_arena_idx);
        let caller = caller as *const _;
        self.caller = unsafe { std::mem::transmute(caller) };
    }

    /// This method cleans up any references to data held within our hook list. This prevents mutable aliasing from
    /// causing UB in our tree.
    ///
    /// This works by cleaning up our references from the bottom of the tree to the top. The directed graph of components
    /// essentially forms a dependency tree that we can traverse from the bottom to the top. As we traverse, we remove
    /// any possible references to the data in the hook list.
    ///
    /// References to hook data can only be stored in listeners and component props. During diffing, we make sure to log
    /// all listeners and borrowed props so we can clear them here.
    ///
    /// This also makes sure that drop order is consistent and predictable. All resources that rely on being dropped will
    /// be dropped.
    pub(crate) fn ensure_drop_safety(&mut self, pool: &ResourcePool) {
        // make sure we drop all borrowed props manually to guarantee that their drop implementation is called before we
        // run the hooks (which hold an &mut Reference)
        // right now, we don't drop
        self.borrowed_props
            .get_mut()
            .drain(..)
            .map(|li| unsafe { &*li })
            .for_each(|comp| {
                // First drop the component's undropped references
                let scope_id = comp
                    .associated_scope
                    .get()
                    .expect("VComponents should be associated with a valid Scope");

                if let Some(scope) = pool.get_scope_mut(scope_id) {
                    scope.ensure_drop_safety(pool);

                    let mut drop_props = comp.drop_props.borrow_mut().take().unwrap();
                    drop_props();
                }
            });

        // Now that all the references are gone, we can safely drop our own references in our listeners.
        self.listeners
            .get_mut()
            .drain(..)
            .map(|li| unsafe { &*li })
            .for_each(|listener| drop(listener.callback.borrow_mut().take()));
    }

    /// A safe wrapper around calling listeners
    pub(crate) fn call_listener(&mut self, event: UserEvent, element: ElementId) {
        let listners = self.listeners.borrow_mut();

        let raw_listener = listners.iter().find(|lis| {
            let search = unsafe { &***lis };
            if search.event == event.name {
                let search_id = search.mounted_node.get();
                search_id.map(|f| f == element).unwrap_or(false)
            } else {
                false
            }
        });

        if let Some(raw_listener) = raw_listener {
            let listener = unsafe { &**raw_listener };
            let mut cb = listener.callback.borrow_mut();
            if let Some(cb) = cb.as_mut() {
                (cb)(event.event);
            }
        } else {
            log::warn!("An event was triggered but there was no listener to handle it");
        }
    }

    /*
    General strategy here is to load up the appropriate suspended task and then run it.
    Suspended nodes cannot be called repeatedly.
    */
    pub(crate) fn call_suspended_node<'a>(&'a mut self, task_id: u64) {
        let mut nodes = self.suspended_nodes.borrow_mut();

        if let Some(suspended) = nodes.remove(&task_id) {
            let sus: &'a VSuspended<'static> = unsafe { &*suspended };
            let sus: &'a VSuspended<'a> = unsafe { std::mem::transmute(sus) };

            let cx: SuspendedContext<'a> = SuspendedContext {
                inner: Context { scope: self },
            };

            let mut cb = sus.callback.borrow_mut().take().unwrap();

            let new_node: Element<'a> = (cb)(cx);
        }
    }

    // run the list of effects
    pub(crate) fn run_effects(&mut self, pool: &ResourcePool) {
        todo!()
        // let mut effects = self.frames.effects.borrow_mut();
        // let mut effects = effects.drain(..).collect::<Vec<_>>();

        // for effect in effects {
        //     let effect = unsafe { &*effect };
        //     let effect = effect.as_ref();

        //     let mut effect = effect.borrow_mut();
        //     let mut effect = effect.as_mut();

        //     effect.run(pool);
        // }
    }

    /// Render this component.
    ///
    /// Returns true if the scope completed successfully and false if running failed (IE a None error was propagated).
    pub(crate) fn run_scope<'sel>(&'sel mut self, pool: &ResourcePool) -> bool {
        // Cycle to the next frame and then reset it
        // This breaks any latent references, invalidating every pointer referencing into it.
        // Remove all the outdated listeners
        self.ensure_drop_safety(pool);

        // Safety:
        // - We dropped the listeners, so no more &mut T can be used while these are held
        // - All children nodes that rely on &mut T are replaced with a new reference
        unsafe { self.hooks.reset() };

        // Safety:
        // - We've dropped all references to the wip bump frame
        unsafe { self.frames.reset_wip_frame() };

        // just forget about our suspended nodes while we're at it
        self.suspended_nodes.get_mut().clear();

        // guarantee that we haven't screwed up - there should be no latent references anywhere
        debug_assert!(self.listeners.borrow().is_empty());
        debug_assert!(self.suspended_nodes.borrow().is_empty());
        debug_assert!(self.borrowed_props.borrow().is_empty());

        log::debug!("Borrowed stuff is successfully cleared");

        // Cast the caller ptr from static to one with our own reference
        let render: &dyn for<'b> Fn(&'b ScopeInner) -> Element<'b> = unsafe { &*self.caller };

        // Todo: see if we can add stronger guarantees around internal bookkeeping and failed component renders.
        if let Some(builder) = render(self) {
            let new_head = builder.into_vnode(NodeFactory {
                bump: &self.frames.wip_frame().bump,
            });
            log::debug!("Render is successful");

            // the user's component succeeded. We can safely cycle to the next frame
            self.frames.wip_frame_mut().head_node = unsafe { std::mem::transmute(new_head) };
            self.frames.cycle_frame();

            true
        } else {
            false
        }
    }
}