# Clippy

Grabador de repeticiones instantáneas para Windows, ultraliviano: graba siempre en segundo plano
(en RAM) y con una tecla (F9) guarda los últimos segundos como MP4. Todo el video pasa por la GPU
(DXGI Desktop Duplication → D3D11 NV12 → encoder H.264 o H.265 por hardware de Media Foundation). Si la placa no
tiene encoder de video, usa el H.264 de Windows por procesador, hasta 1080p a 30 fps.

Descarga y novedades en [clpy.vercel.app](https://clpy.vercel.app). Las versiones se publican en
los [Releases](https://github.com/MorguiMateo/clippy/releases) de este repo, que es donde las busca
el actualizador automático.

## Compilar

```bash
cargo build --release
```

El exe queda en `target/release/clippy.exe`. Para publicar se renombra a `Clippy.exe`
(el actualizador busca ese nombre exacto en el último release de `MorguiMateo/clippy`).

Para probar mientras desarrollás, con los modos de prueba sin ventana (no van en el exe publicado).
Compila en segundos porque no usa LTO; queda en `target/fast/clippy.exe`:

```bash
cargo build --profile fast --features devtools
```

## Modos del mismo .exe

| Argumentos | Qué hace |
| --- | --- |
| (ninguno) / `--background` | Grabadora con ícono en la bandeja (`--background`: sin abrir la ventana, para el inicio con Windows). |
| `--settings` | Ventana principal / ajustes. |
| `--editor [archivo]` | Editor de clips. |
| `--apply-update <ruta>` | Lo usa el actualizador: reemplaza el exe instalado y lo vuelve a abrir. |

Solo con `--features devtools`:

| Argumentos | Qué hace |
| --- | --- |
| `--test <seg> <carpeta> [game\|desktop\|mic\|juegos\|cursor\|hevc\|720\|1080\|1440]` | Graba, guarda un clip y sale (imprime el encoder y el juego detectado; `cursor` dibuja el cursor del mouse; `hevc` graba en H.265; un número elige la resolución del clip). Con la variable `CLIPPY_CPU_ENCODER=1`, codifica con el procesador como en una placa sin encoder; con `CLIPPY_TEST_LOST=<seg>`, a esos segundos se pierde la placa una vez, como en un reinicio del driver; con `CLIPPY_TEST_STALL=<seg>`, la captura se traba pasado ese tiempo. |
| `--test-fullscreen <ancho>x<alto> <seg> [alttab]` | Un juego de mentira en pantalla completa exclusiva (cambia el modo de video, como una resolución estirada); con `alttab`, sale y vuelve cada 6 s. Corré una copia del exe con otro nombre para que cuente como juego. |
| `--test-soak <minutos> <carpeta> [escenario\|-] [segundos entre clips]` | Graba un rato largo midiéndose a sí misma y deja un informe de rendimiento y fugas (ver [Medir el rendimiento](#medir-el-rendimiento)). |
| `--test-mic` | Cuánto audio entrega cada micrófono en 3 s. |
| `--test-download` | Descarga la versión publicada como el botón "Actualizar". |
| `--test-export`, `--test-frame`, `--test-strip` | Pruebas del editor (ver `src/editor/devtools.rs`). |
| `--settings` o `--editor` con `CLIPPY_TEST_WINDOW=<ancho>x<alto>` | La ventana se abre de ese tamaño y sin maximizar, para ver cómo queda angosta (`760x560` es la de ajustes más chica). |

## Medir el rendimiento

Hay dos formas de medir. Las dos son solo para Windows.

- **`--test-soak`**: la grabadora se mide a sí misma mientras graba y busca fugas. Es la completa, pero solo existe en el
  exe con `devtools`.
- **`tools\medir.ps1`**: mide desde afuera al Clippy que ya tenés abierto. Sirve con cualquier versión, también con las
  anteriores a `--test-soak`, para comparar.

### Con Claude Code (lo más simple)

Abrí Claude Code en la PC con Windows, en este repo, y pedile algo como:

> medí el rendimiento 15 minutos con un juego y buscá fugas

> compará el rendimiento contra la 0.4.5

Claude usa la skill `rendimiento` (`.claude/skills/rendimiento/SKILL.md`): compila, corre la prueba aislada y la vigila en
vivo. Cada minuto ve una línea `ESTADO` y, en cuanto algo se sale de lo normal, una línea `AVISO`. Al final lee el informe
y te dice qué anda mal y en qué archivo mejorarlo.

Otros pedidos que entiende:

> medí 10 minutos el escritorio con el encoder por procesador

> grabá 30 minutos en "Solo juegos" y fijate si pierde memoria

> medí si este cambio mejoró el procesador comparado con main

Lo que necesita la PC:
- [Rust](https://rustup.rs) con las herramientas de compilación de Visual Studio (el instalador de rustup las ofrece).
- [Git para Windows](https://git-scm.com/download/win), que trae Git Bash.
- Claude Code.

La primera compilación tarda un par de minutos.

### A mano, con `--test-soak`

1. **Conseguí el exe con `devtools`**. Hay dos formas:
   - Compilarlo en este repo: `cargo build --profile fast --features devtools`. Queda en `target/fast/clippy.exe`.
   - Bajarlo sin compilar: en GitHub → Actions → **Build** → la última corrida de *Run workflow* → artefacto
     `Clippy-devtools.exe`. No es el exe publicado: no lo uses para jugar.
2. **Cerrá tu Clippy** (clic derecho en la bandeja → Salir) y lo que use mucho procesador o placa: si no, los números se
   mezclan.
3. **Corré la prueba en Git Bash**, cada una en una carpeta nueva y con su propio `APPDATA` (así usa los ajustes de
   fábrica y no toca los tuyos):

   ```bash
   D="$TEMP/clippy-perf/prueba1"; mkdir -p "$D/datos"
   APPDATA="$D/datos" target/fast/clippy.exe --test-soak 10 "$D" desktop 60 > "$D/consola.txt" 2>&1
   ```

   Los argumentos son `<minutos> <carpeta> [escenario|-] [segundos entre clips]`:
   - **Minutos**: para buscar fugas, 10 o más. Para comparar consumo alcanza con 3.
   - **Escenario**:
     - `desktop`: el escritorio y el audio del sistema.
     - `game`: el canal del juego.
     - `mic`: el micrófono.
     - `juegos`: el modo "Solo juegos".
     - `cursor`: el cursor dibujado.
     - `hevc`: H.265.
     - `720`, `1080` o `1440`: la resolución del clip.
     - `-`: el de fábrica.
   - **Segundos entre clips**: cada cuánto guarda uno (60 si no se dice).
   - **Encoder por procesador**: poné `CLIPPY_CPU_ENCODER=1` antes del comando, como el `APPDATA`.
   - **Con un juego**: corré a la par el juego de mentira desde una copia del exe con otro nombre. Cambia la resolución
     del monitor mientras dura.

     ```bash
     cp target/fast/clippy.exe "$D/juego.exe"
     "$D/juego.exe" --test-fullscreen 1920x1080 660 &
     ```

4. **Seguila mientras graba**: en otra ventana de Git Bash, `tail -f "$D/consola.txt"`. Cada minuto sale una línea
   `ESTADO` y, si algo se sale de lo normal, una línea `AVISO`:
   - el encoder recibe menos cuadros de los pedidos;
   - se saltean cuadros;
   - una vuelta del hilo de grabación tarda más que un cuadro;
   - la RAM salta sin guardar un clip.
5. **Leé el resultado** en la carpeta:
   - `informe.md`: el veredicto (`OK` o `REVISAR`) y los avisos, más el consumo grabando y al guardar (procesador, placa
     por motor, RAM, memoria de video), los cuadros por segundo reales, cuánto se frena la grabación al guardar y la
     tabla de fugas.
   - `informe.json`: lo mismo, para comparar corridas.
   - `perf.jsonl`: una muestra por segundo.

   El exe sale con 0 si está todo bien, con 3 si hay algo para revisar y con 1 si no pudo medir.

**Cómo se buscan las fugas**:
- Desde que se llena el buffer, se toma el valor más bajo de cada tramo de 30 s (así no cuentan los picos de guardar un
  clip). Si eso sigue subiendo, el informe lo marca como "posible fuga".
- Se revisan la RAM privada, la RAM fuera de los buffers de video y audio, la memoria de video, los handles, los hilos y
  los objetos de Windows.
- Si sube la RAM privada pero no la de fuera de los buffers, no es una fuga: es el buffer, que pesa más cuando el video se
  mueve más.
- Al final se ve si quedaron hilos o handles sin soltar al cerrar la grabadora.

Medir cuesta alrededor de 1 % de un núcleo, sobre todo por los contadores de la placa. El informe dice cuánto fue y ya lo
descuenta.

### Desde afuera, con `tools\medir.ps1`

Con Clippy abierto y grabando, en la situación que quieras medir:

```powershell
powershell -ExecutionPolicy Bypass -File tools\medir.ps1 -Nombre 0.4.6
```

Aprieta sola tu tecla de guardar clip (guarda clips de verdad) y mide procesador, RAM, memoria de video y uso de la placa
antes, durante y después de cada uno. Deja en el Escritorio, en `clippy-medicion\`, un CSV por corrida (una fila por
segundo) y `resumen.csv` (una fila por corrida). Para comparar versiones, corré lo mismo con cada exe y mirá
`resumen.csv`. Opciones:
- `-Clips 5`: cuántos clips guarda.
- `-Antes 30`: segundos de grabación normal antes de cada uno.
- `-Despues 5`: segundos que sigue midiendo después de cada uno.

## Clippy contra Bals

`tools\versus.ps1` compara Clippy con [Bals](https://github.com/sconelli24/bals), otra grabadora de repeticiones, en tu
PC y en las mismas condiciones. Graba con cada uno por turnos, guarda un clip cada 30 s con su propia tecla y mide desde
afuera:
- procesador, RAM y memoria de video;
- cada motor de la placa;
- handles e hilos;
- cuánto tarda en salir el clip y cómo sale (bitrate, cuadros por segundo, duración).

Bals no está en este repo, porque no tiene licencia. El script baja el `bals.exe` de su última versión la primera vez.

### Con Claude Code

Abrí Claude Code en esta carpeta y pedile:

> Hacé un versus de Clippy contra Bals con 2 vueltas y decime dónde pierde Clippy y qué cambiarías para mejorarlo.

Usa la skill `versus` (`.claude/skills/versus/`), que es el ciclo completo:
1. Compila y corre el versus.
2. Lee el resultado.
3. Para cada cosa en la que Clippy pierde, busca la parte del código que la hace y mira cómo la resuelve Bals.
4. Propone el cambio. Si le decís que lo haga, lo hace en una rama y vuelve a medir para confirmar que mejoró.

### A mano

Con Clippy y Bals cerrados (el script abre los suyos, con carpetas de datos propias: no toca tus ajustes ni tus clips):

```powershell
cargo build --release
powershell -ExecutionPolicy Bypass -File tools\versus.ps1 -Vueltas 2
```

Mientras mide, una ventana a pantalla completa cambia en cada cuadro, como un juego. No la cierres ni uses la PC.
Tarda unos 15 minutos con 2 vueltas de 3 minutos. Deja en el Escritorio, en `clippy-versus\<fecha>\`:
- `versus.md`: la tabla, con la diferencia en % y quién gana cada fila.
- `versus.json`: los mismos datos.
- Una carpeta por corrida, con una muestra por segundo (`muestras.csv`) y el registro de cada programa.

Opciones:
- `-Ajustes fabrica`: cada uno como viene. A Bals se le pone grabar el monitor, porque de fábrica solo graba juegos.
  Sin la opción (`igualados`), los dos graban a 1080p, 60 fps, ~10 Mbps, sin micrófono ni cursor.
- `-Escena quieto`: el escritorio sin movimiento, en vez de la ventana que cambia.
- `-Minutos 5`: minutos que graba cada uno por vuelta.
- `-Vueltas 3`: cuántas vueltas. Alterna quién va primero y promedia.
- `-Clippy <exe>` / `-Bals <exe>`: otra versión de cada uno, por ejemplo tu rama contra `main`.

Para los cuadros por segundo del clip hace falta `ffprobe` (`winget install ffmpeg`). Sin él, el resto se mide igual.

## Mapa del código

```
src/
  main.rs          elige el modo
  config.rs        config.json (grabación, monitor, audio, atajos globales)
  log.rs           %APPDATA%\Clippy, clippy.log y marcas de tiempo
  mf.rs            Media Foundation y el formato de audio de toda la app (PCM 48 kHz estéreo, AAC)
  clipcache.rs     caché de forma de onda y fotogramas clave por clip (la escribe la grabadora, la lee el editor)
  app/             el proceso como app de Windows
    mode.rs        modos, argumentos, candados, títulos de ventana, mensajes a la bandeja
    tray.rs        bandeja, atajos globales, controla la grabadora
    update.rs      actualizaciones (una consulta por día a Releases, como mucho)
    status.rs      recorder.json: con qué graba y por qué no puede (lo lee la ventana principal)
    autostart.rs   inicio con Windows (HKCU\...\Run)
    keys.rs        nombres de teclas Win32
  record/          grabación
    recorder.rs    hilo de grabación: captura -> encoder -> buffer -> guardar
    game.rs        qué es "el juego" (ventana en pantalla completa), para video y audio
    cursor.rs      el cursor del mouse sobre la imagen del escritorio (en la GPU, antes de pasarla a NV12)
    track.rs       la pista de audio de los clips: mezcla los canales y la comprime a AAC (aac.rs) mientras graba
    capture.rs, convert.rs, encoder.rs, audio.rs, mux.rs, clock.rs
  settings/        ventana principal (mod.rs: estado; pages.rs: páginas)
  editor/          editor de clips (proceso aparte)
    mod.rs         ventana y bucle principal
    timeline.rs    pistas y piezas (video y audio unidos), imán, fundidos, deshacer
    playback.rs    reproducción: un reproductor por pieza que se ve o se oye, capas y fundidos
    export.rs      diálogo y trabajo de exportación
    panels.rs      barra de arriba y línea de tiempo (pistas, marcadores, clic derecho en un clip)
    browser.rs     panel de la biblioteca: elegir, carpetas, mover y mandar a la Papelera
    inspector.rs   panel del clip: imagen, velocidad y sonido
    preview.rs     vista previa: lienzo con su proporción y recorte de la imagen
    prefs.rs       editor.json (atajos, últimas opciones de exportación, paneles, imán)
    media.rs       exportar: copia sin recodificar o capas ubicadas y fundidas por GPU, velocidad, mezcla de audio
    shortcuts.rs, library.rs, player.rs, devtools.rs
  ui_kit/          estilo y widgets, ventanas nativas, íconos, sonidos
    watch.rs       despierta una ventana cuando algo cambia afuera (sin repintar en reposo)
```

## Archivos del usuario

Todo en `%APPDATA%\Clippy`: `config.json`, `editor.json`, `library.json`, `update.json`,
`clippy.log` y `cache\`.

## Publicar una versión

1. Subir la versión en `Cargo.toml`.
2. En GitHub → Actions → **Build** → *Run workflow*, con el tag (por ejemplo `v0.3.0`) y
   "Publicar el release" tildado. El workflow compila y sube `Clippy.exe` al release de este repo
   (si no existe, lo crea como borrador). Sin tildarlo, igual compila y deja el exe como artefacto descargable.
3. Escribir el texto del release para quien usa la app (Clippy lo muestra como novedades: títulos `#`,
   viñetas `-` y párrafos, sin `**` ni comillas invertidas) y recién ahí publicarlo. Un release publicado
   sin texto no muestra novedades.

## Licencia

[GPL-3.0-or-later](LICENSE). Podés usar, estudiar, cambiar y redistribuir Clippy; si distribuís una
versión modificada, tiene que ser con su código y bajo la misma licencia. El nombre "Clippy" y su
ícono no se pueden usar para identificar versiones modificadas.
