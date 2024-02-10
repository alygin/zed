use gpui::{
    actions, rems, AppContext, DismissEvent, EventEmitter, FocusHandle, FocusableView,
    ParentElement, Render, Styled, Task, View, ViewContext, VisualContext, WeakView,
};
use picker::{Picker, PickerDelegate};
use std::sync::{atomic::AtomicBool, Arc};
use ui::{prelude::*, HighlightedLabel, ListItem, ListItemSpacing};
use util::ResultExt;
use workspace::{ModalView, Workspace};

actions!(item_finder, [Toggle]);

pub struct ItemFinder {
    picker: View<Picker<ItemFinderDelegate>>,
}

impl ModalView for ItemFinder {}

pub fn init(cx: &mut AppContext) {
    cx.observe_new_views(ItemFinder::register).detach();
}

impl ItemFinder {
    fn register(workspace: &mut Workspace, _: &mut ViewContext<Workspace>) {
        workspace.register_action(|workspace, _: &Toggle, cx| {
            let Some(item_finder) = workspace.active_modal::<Self>(cx) else {
                Self::open(workspace, cx);
                return;
            };

            item_finder.update(cx, |item_finder, cx| {
                item_finder
                    .picker
                    .update(cx, |picker, cx| picker.cycle_selection(cx))
            });
        });
    }

    fn open(workspace: &mut Workspace, cx: &mut ViewContext<Workspace>) {
        workspace.toggle_modal(cx, |cx| {
            // workspace.active_pane().
            let delegate = ItemFinderDelegate::new(
                cx.view().downgrade(),
                vec![
                    "consts.rs",
                    "zed - fish",
                    "Pane::new",
                    "number.rs",
                    "fibonacci.rs",
                    "lib.rs",
                ],
                cx,
            );
            ItemFinder::new(delegate, cx)
        });
    }

    fn new(delegate: ItemFinderDelegate, cx: &mut ViewContext<Self>) -> Self {
        Self {
            picker: cx.new_view(|cx| Picker::new(delegate, cx)),
        }
    }
}

impl EventEmitter<DismissEvent> for ItemFinder {}

impl FocusableView for ItemFinder {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ItemFinder {
    fn render(&mut self, _cx: &mut ViewContext<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

pub struct ItemFinderDelegate {
    item_finder: WeakView<ItemFinder>,
    selected_index: usize,
    cancel_flag: Arc<AtomicBool>,
    items: Vec<&'static str>,
}

impl ItemFinderDelegate {
    fn new(
        item_finder: WeakView<ItemFinder>,
        items: Vec<&'static str>,
        cx: &mut ViewContext<ItemFinder>,
    ) -> Self {
        // cx.observe(&project, |item_finder, _, cx| {
        //     // TODO: We should probably not re-render on every project anything
        //     item_finder
        //         .picker
        //         .update(cx, |picker, cx| picker.refresh(cx))
        // })
        // .detach();

        Self {
            item_finder,
            selected_index: 0,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            items,
        }
    }
}

impl PickerDelegate for ItemFinderDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self) -> Arc<str> {
        "Search opened tabs...".into()
    }

    fn match_count(&self) -> usize {
        self.items.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, cx: &mut ViewContext<Picker<Self>>) {
        self.selected_index = ix;
        cx.notify();
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        Vec::new()
    }

    fn update_matches(
        &mut self,
        raw_query: String,
        cx: &mut ViewContext<Picker<Self>>,
    ) -> Task<()> {
        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, cx: &mut ViewContext<Picker<ItemFinderDelegate>>) {}

    fn dismissed(&mut self, cx: &mut ViewContext<Picker<ItemFinderDelegate>>) {
        self.item_finder
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut ViewContext<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let item_match = self
            .items
            .get(ix)
            .expect("Invalid matches state: no element for index {ix}");

        Some(
            ListItem::new(ix)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .selected(selected)
                .child(h_flex().gap_2().child(HighlightedLabel::new(
                    SharedString::from(*item_match),
                    Vec::new(),
                ))),
        )
    }
}
