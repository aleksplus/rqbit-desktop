//! Browser file picker: an `<input type="file">` shim bridged into Rust.
//!
//! GPUI has no file-dialog support on wasm, so the picker is a plain DOM
//! input element: create it hidden, click it programmatically, and read the
//! picked files as bytes when the `change` event fires (`cancel` resolves
//! with an empty list). The element is removed from the DOM after either
//! event.

use std::cell::Cell;
use std::rc::Rc;

use futures::channel::oneshot;
use js_sys::Uint8Array;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// A file picked through the browser picker.
pub struct PickedFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Open the browser file picker. Resolves `Ok(vec![])` when the user
/// cancels.
pub fn pick_files(
    accept: &str,
    multiple: bool,
) -> oneshot::Receiver<Result<Vec<PickedFile>, String>> {
    let (tx, rx) = oneshot::channel::<Result<Vec<PickedFile>, String>>();
    // `oneshot::Sender` is not Clone; the change/cancel closures and the
    // setup error path share it through this cell and `take()` it on use.
    let tx = Rc::new(std::cell::RefCell::new(Some(tx)));

    let setup = (|| -> Result<(), String> {
        let document = web_sys::window()
            .ok_or("no window")?
            .document()
            .ok_or("no document")?;
        let input: web_sys::HtmlInputElement = document
            .create_element("input")
            .map_err(|_| "could not create the file input".to_string())?
            .dyn_into()
            .map_err(|_| "could not create the file input".to_string())?;
        input.set_type("file");
        input.set_accept(accept);
        input.set_multiple(multiple);
        input
            .set_attribute("style", "display:none")
            .map_err(|_| "could not hide the file input".to_string())?;
        document
            .body()
            .ok_or("no body")?
            .append_child(&input)
            .map_err(|_| "could not attach the file input".to_string())?;

        // One-shot flag: whichever of change/cancel fires first resolves the
        // picker; the other becomes a no-op.
        let resolved = Rc::new(Cell::new(false));

        let send = {
            let tx = tx.clone();
            move |value: Result<Vec<PickedFile>, String>| {
                if let Some(tx) = tx.borrow_mut().take() {
                    let _ = tx.send(value);
                }
            }
        };

        let on_change = {
            let input = input.clone();
            let resolved = resolved.clone();
            let send = send.clone();
            Closure::<dyn FnMut(JsValue)>::new(move |_event: JsValue| {
                if resolved.replace(true) {
                    return;
                }
                let input = input.clone();
                let send = send.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let result = read_files(&input).await;
                    let _ = input.remove();
                    send(result);
                });
            })
        };
        let on_cancel = {
            let input = input.clone();
            let resolved = resolved.clone();
            Closure::<dyn FnMut(JsValue)>::new(move |_event: JsValue| {
                if resolved.replace(true) {
                    return;
                }
                let _ = input.remove();
                send(Ok(Vec::new()));
            })
        };

        input
            .add_event_listener_with_callback("change", on_change.as_ref().unchecked_ref())
            .map_err(|_| "could not listen for file picks".to_string())?;
        input
            .add_event_listener_with_callback("cancel", on_cancel.as_ref().unchecked_ref())
            .map_err(|_| "could not listen for picker cancel".to_string())?;
        // The listeners must outlive the pick; the input is removed from the
        // DOM once one fires, so the (tiny) leaked closures go dead with it.
        on_change.forget();
        on_cancel.forget();

        input.click();
        Ok(())
    })();

    if let Err(message) = setup
        && let Some(tx) = tx.borrow_mut().take()
    {
        let _ = tx.send(Err(message));
    }
    rx
}

/// Read every file currently selected on the input as bytes.
async fn read_files(input: &web_sys::HtmlInputElement) -> Result<Vec<PickedFile>, String> {
    let files = input.files().ok_or("no file list")?;
    let mut picked = Vec::new();
    for index in 0..files.length() {
        let file = files
            .get(index)
            .ok_or_else(|| format!("file {index} disappeared"))?;
        let name = file.name();
        let buffer = JsFuture::from(file.array_buffer())
            .await
            .map_err(|_| format!("could not read {name}"))?;
        let bytes = Uint8Array::new(&buffer).to_vec();
        picked.push(PickedFile { name, bytes });
    }
    Ok(picked)
}
