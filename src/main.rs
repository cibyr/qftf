use fltk::frame::Frame;
use fltk::image::SvgImage;
use fltk::{app, prelude::*, window::Window};
use std::env;
use qrcode::QrCode;
use qrcode::render::svg;

fn main() {
    let args: Vec<String> = env::args().collect();
    let code_string = &args[1];

    let code = QrCode::new(code_string).unwrap();
    let svg = code.render::<svg::Color>()
        .min_dimensions(400, 400)
        .build();

    let app = app::App::default();
    let mut wind = Window::new(100, 100, 400, 400, "QFT");

    let mut frame = Frame::default().with_size(400, 400).center_of(&wind);
    let image = SvgImage::from_data(&svg).unwrap();
    frame.set_image(Some(image));

    wind.end();
    wind.show();
    app.run().unwrap();
}
