# Desktop Widgets

macOS / iOS-style desktop widgets for Windows, written in Rust. They have a "liquid glass" look and draw themselves on the CPU, with no UI framework. They sit on the desktop above the wallpaper and below every other window.

> [!WARNING]
> **This is an unsupported personal side project.**
>
> I built this for fun, for my own laptop, and I currently have **no plans to support or maintain it**. I won't be answering issues, fixing bugs or reviewing pull requests.
>
> **It almost certainly won't work properly on your PC as it is.** Many things are hard-coded for my machine and setup (see [What probably won't work on your PC](#what-probably-wont-work-on-your-pc)). To make it work for you, expect to read the code and change it yourself. Feel free to fork it and do whatever you like with it.

## What's in it

- **Clock:** a big, tall, centred clock with the date underneath.
- **Weather:** the current temperature and condition with an Apple Weather-style icon, from [Open-Meteo](https://open-meteo.com/) (free, no API key).
- **Music:** album art, title, artist, a draggable progress bar and previous / play-pause / next buttons, through Windows' media controls. It includes spring animations, a slide between tracks and art that shrinks when paused.
- **Wi-Fi and Bluetooth:** macOS Control Center-style cards. Click to turn the radio on or off.
- **Volume and brightness:** tall iOS-style sliders. Drag or scroll to change them. Clicking the speaker opens a per-app **volume mixer** that stretches out of the pill.
- **Battery:** a wide pill with a macOS-style battery icon, a level bar and the time remaining or charging status.
- **Stocks:** the S&P 500, NVIDIA, Apple, Microsoft, Amazon, Alphabet and Meta, with price and daily change. Prices refresh every 5 minutes while the US market is open and not at all while it's closed.

Every card is drawn on "liquid glass": the wallpaper behind it is bent through a lens, with chromatic aberration, a soft blur and a specular rim light.

### Performance

I cared a lot about keeping it light:

- When nothing is moving, nothing redraws. A widget only draws when something it shows changes, plus a check every couple of seconds for a wallpaper change.
- Animations are synced to the display's refresh, using its frame timing, so they stay smooth.
- On my machine it idles at well under 1% of one CPU core and uses about 70 MB of RAM.

## What probably won't work on your PC

These are the things I know are specific to my setup. There are likely others.

| Area | The problem | Where to change it |
| --- | --- | --- |
| **Fonts** | The fonts aren't in this repo (licensing), so **it won't build until you add them**. | See [Fonts](#fonts) |
| **Layout** | Tuned for one 2560×1600 (16:10) laptop screen. Sizes and positions scale with screen height, but other aspect ratios, resolutions or scaling may overlap or run off the screen. Only the primary monitor is used. | The `*_FRACTION` constants at the top of each widget's file, and `main.rs` |
| **Music player** | Only follows **Spotifast** (the Spotify client I use), so other players are ignored on purpose. | `APP_FILTER` in [`src/media.rs`](src/media.rs), e.g. `"spotify"` |
| **Brightness** | Uses the laptop's built-in display controls (WMI). External monitors and most desktop PCs won't respond. | [`src/levels.rs`](src/levels.rs) |
| **Battery** | Desktop PCs just show "No Battery". | [`src/battery.rs`](src/battery.rs), or remove it in `main.rs` |
| **Glass effect** | Reads your wallpaper image to draw what's behind the glass. Slideshows, Windows Spotlight, animated wallpapers or multiple monitors may make it look wrong. | [`src/wallpaper.rs`](src/wallpaper.rs) |
| **Stocks** | Uses Yahoo Finance's public (unofficial) endpoint, which could change or stop working at any time. The symbols are hard-coded. | `SYMBOLS` in [`src/stocks.rs`](src/stocks.rs) |
| **Windows version** | Only ever run on Windows 11, on one laptop. | — |

## Building

You need:

- **Windows 10/11**
- **Rust** 1.85 or newer (the project uses the 2024 edition). Install it from [rustup.rs](https://rustup.rs/).
- **The fonts** (see below)

```powershell
git clone <this repo>
cd widgets
# put the fonts in assets\ first (see below)
cargo build --release
.\target\release\widgets.exe
```

### Fonts

The fonts are compiled into the program, and you need to supply them yourself. Put these files in the [`assets/`](assets/) folder with exactly these names:

| File | Used for | Where to get it |
| --- | --- | --- |
| `Pillar.ttf` | The big clock digits | Search for the "Pillar" font. Any font works if you rename it to this. |
| `SF-Pro-Display-Regular.otf` | Most text | [Apple's SF Pro](https://developer.apple.com/fonts/) |
| `SF-Pro-Display-Medium.otf` | Numbers and names | Apple's SF Pro |
| `SF-Pro-Display-Bold.otf` | Titles | Apple's SF Pro |
| `SF-Pro-Display-Light.otf` | Light text | Apple's SF Pro |

Check the licence of whatever fonts you use. If you'd rather use other fonts (for example [Inter](https://rsms.me/inter/)), rename them to match or change the `include_bytes!` paths in [`src/render.rs`](src/render.rs).

## Using it

- **Right-click** any widget for its menu, which has **Quit**. The clock's menu toggles 12/24-hour time. The weather's menu has **Set city…** and **Refresh now**.
- **Weather city:** use **Set city…**, which opens the settings file in Notepad. The weather updates once you save and close it.
- **Settings file:** `%APPDATA%\widgets\config.toml`

  ```toml
  clock_24h = false
  city = "London"
  ```

### Starting with Windows

The simplest way is a shortcut to `target\release\widgets.exe` in your Startup folder. Press <kbd>Win</kbd>+<kbd>R</kbd>, type `shell:startup`, then paste the shortcut there.

Admin rights aren't needed.

## How it works (briefly)

- Each widget is a bare layered window (`UpdateLayeredWindow` with a premultiplied BGRA bitmap). It is never activated by clicks and is kept at the bottom of the window stack, so it acts like part of the desktop.
- Everything is drawn on the CPU: text with `ab_glyph`, and shapes and icons as anti-aliased signed distance functions.
- Anything slow (network, media sessions, audio, radios) runs on background threads that post a message to the window when something changes.
- Animations use damped springs, timed by the clock rather than by frames, so a new click takes over smoothly mid-animation.

## Credits

- The liquid glass effect follows AmirHossein Aghajari's [breakdown of Apple's Liquid Glass](https://medium.com/@aghajari/liquid-glass-ios-effect-explanation-dabadd6414ae).
- Weather data from [Open-Meteo](https://open-meteo.com/).
- Stock prices from Yahoo Finance's public chart endpoint.
- The design is inspired by Apple's macOS and iOS widgets. This project isn't affiliated with Apple in any way.

## License

[MIT](LICENSE). Do whatever you want with it. It comes with no warranty and no support.
