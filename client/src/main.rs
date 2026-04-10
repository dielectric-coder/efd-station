mod ws;
mod ui;

use std::sync::{Arc, Mutex};

use efd_proto::{FftBins, RadioState, ServerMsg};
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow};

const APP_ID: &str = "com.dielectriccoder.efd-client";

fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ws://127.0.0.1:8080/ws".to_string());

    let app = Application::builder().application_id(APP_ID).build();

    let url2 = url.clone();
    app.connect_activate(move |app| build_ui(app, &url2));

    app.run_with_args::<String>(&[]);
}

fn build_ui(app: &Application, url: &str) {
    // Shared state — written by WS thread, read by GTK draw funcs
    let fft_data: Arc<Mutex<Option<FftBins>>> = Arc::new(Mutex::new(None));
    let radio_state: Arc<Mutex<Option<RadioState>>> = Arc::new(Mutex::new(None));

    // Message queue from WS thread to GTK main loop
    let msg_queue: Arc<Mutex<Vec<ServerMsg>>> = Arc::new(Mutex::new(Vec::new()));

    // Start WS connection
    let ws_tx = ws::start(url, msg_queue.clone());

    // Build window
    let window = ApplicationWindow::builder()
        .application(app)
        .title("efd-station")
        .default_width(1024)
        .default_height(700)
        .build();

    let main_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let controls = ui::controls::Controls::new(ws_tx.clone());
    main_box.append(controls.widget());

    let spectrum = ui::spectrum::Spectrum::new(fft_data.clone());
    spectrum.widget().set_vexpand(true);
    main_box.append(spectrum.widget());

    let waterfall = ui::waterfall::Waterfall::new();
    waterfall.widget().set_vexpand(true);
    main_box.append(waterfall.widget());

    window.set_child(Some(&main_box));

    // Poll message queue from GTK main loop (60 fps tick)
    let fft_data2 = fft_data.clone();
    let radio_state2 = radio_state.clone();
    let spectrum2 = spectrum.clone();
    let waterfall2 = waterfall.clone();
    let controls2 = controls.clone();
    let queue = msg_queue.clone();

    glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
        let msgs: Vec<ServerMsg> = {
            let mut q = queue.lock().unwrap();
            q.drain(..).collect()
        };

        let mut need_redraw = false;

        for msg in msgs {
            match msg {
                ServerMsg::FftBins(bins) => {
                    waterfall2.push_line(&bins.bins);
                    *fft_data2.lock().unwrap() = Some(bins);
                    need_redraw = true;
                }
                ServerMsg::RadioState(state) => {
                    controls2.update(&state);
                    *radio_state2.lock().unwrap() = Some(state);
                }
                ServerMsg::Audio(_) => {}
                ServerMsg::Error(err) => {
                    eprintln!("server error: [{}] {}", err.code, err.message);
                }
            }
        }

        if need_redraw {
            spectrum2.queue_draw();
            waterfall2.queue_draw();
        }

        glib::ControlFlow::Continue
    });

    window.present();
}
