use adw::prelude::*;
use adw::{Application, ApplicationWindow, HeaderBar, WindowTitle};
use gtk::{Box as GtkBox, Label, Orientation};

const APP_ID: &str = "app.openbubbles.Gtk.Devel";

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &Application) {
    let header = HeaderBar::builder()
        .title_widget(&WindowTitle::new("OpenBubbles", "scaffold"))
        .build();

    let label = Label::builder()
        .label("Native GTK4 / libadwaita client — scaffold")
        .vexpand(true)
        .build();

    let content = GtkBox::new(Orientation::Vertical, 0);
    content.append(&header);
    content.append(&label);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("OpenBubbles")
        .default_width(420)
        .default_height(320)
        .content(&content)
        .build();

    window.present();
}
