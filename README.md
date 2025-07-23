# QFTF

Sometimes you have two computers next to each other and you just want to get a file from one to the other without emailing it to yourself or putting it on a USB stick. Sure, you could use the excellent [Magic Wormhole](https://github.com/magic-wormhole/magic-wormhole), but it's, like, a whole Python thing to install and they expect you to type these codes in... there's gotta be a better way! Introducing...

## QR code File TransFer

Because "QFT" was already taken. Uses [iroh](https://www.iroh.computer/) for p2p file transfer between two computers. The two endpoints learn about each other via iroh running in wasm on a web page on your phone [qftf-web](https://github.com/cibyr/qftf-web/). The public keys and other details are encoded in QR codes you scan on your phone, so you don't have to type anything.

## Installation

`cargo install qftf`

TODO: provide pre-built binaries

## Usage

TODO: put some screenshots here

1. Run `qftf some-file-you-want-to-send` on the computer that has the file. It'll pop up a window with a QR code.
2. Run `qftf` (with no arguments) on the other computer. It'll also pop up a QR code.
3. Scan either one of the QR codes with your phone, and open the link.
4. Hit the "Start Camera" button on the page that appears and scan the other QR code.
5. Hit the "Go" button.
6. Magic happens!

## Configuration

You can set the `QFTF_URL_PREFIX` environment variable to use a different base URL for the generated QR codes (e.g. if you're hacking on the web UI you probably want to point it at your own server). The default value is `https://cibyr.github.io/qftf-web/`.
