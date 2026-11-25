---
name: versus
description: Comparar el rendimiento de Clippy contra Bals (github.com/sconelli24/bals, otra grabadora de repeticiones en Rust) en la misma PC con tools/versus.ps1, y usar el resultado para mejorar Clippy - encontrar dónde pierde, ver cómo lo resuelve Bals, cambiarlo y volver a medir. Usala cuando pidan un versus, comparar con Bals o con "el programa de mi amigo", o mejorar Clippy contra la competencia.
---

# Clippy contra Bals

`tools/versus.ps1` graba con cada programa por turnos y en las mismas condiciones, guarda un clip cada tanto con la
tecla de cada uno y mide desde afuera: procesador, RAM, memoria de video, cada motor de la placa, handles, hilos,
cuánto tarda en salir el clip y cómo sale (bitrate, cuadros por segundo, duración). Mide igual a cualquier versión de
los dos, sin compilar nada especial.

Todo es de Windows. Bals no se copia al repo (no tiene licencia): el script baja su `bals.exe` de la última versión
publicada a `<salida>\bin\` y lo usa desde ahí.

## 1. Correr el versus

Requisitos: Clippy y Bals **cerrados** (el script abre los suyos, con carpetas de datos propias, y no toca los ajustes
ni los clips del usuario), nada pesado abierto, y `ffprobe` en el PATH para medir los cuadros del clip
(`winget install ffmpeg`; sin él igual corre). La escena de movimiento tapa la pantalla: avisale al usuario.

```bash
cargo build --release                      # o pasá -Clippy <ruta a otro exe>
powershell -ExecutionPolicy Bypass -File tools/versus.ps1 -Minutos 3 -Vueltas 2 > "$TEMP/versus.txt" 2>&1
```

Opciones:
- `-Ajustes igualados` (de fábrica en el script): los dos a 1080p, 60 fps, ~10 Mbps, sin micrófono ni cursor, con el
  audio del escritorio. Es la comparación justa del motor. `-Ajustes fabrica`: como vienen (a Bals se le pone grabar el
  monitor, porque de fábrica solo graba juegos y en el escritorio no haría nada).
- `-Escena movimiento` (de fábrica): una ventana a pantalla completa que cambia en cada cuadro, como un juego.
  `-Escena quieto`: el escritorio quieto, donde las dos se saltean casi todo el trabajo.
- `-Vueltas 2` o más: alterna quién va primero y promedia. Hacelo siempre que vayas a sacar conclusiones.
- `-Clippy <exe>` / `-Bals <exe>`: otra versión de cada uno (por ejemplo, el exe de `main` contra el de tu rama).
- `-Minutos`, `-CadaSeg` (segundos entre clips), `-Salida` (de fábrica, `Escritorio\clippy-versus`).

Tarda `Vueltas × 2 × (Minutos + ~0,5)` minutos. Lanzalo en segundo plano y seguí el archivo de salida.

## 2. Leer el resultado

En `<salida>\<fecha>-<ajustes>-<escena>\`:
- `versus.md`: la tabla Clippy contra Bals, con diferencia en % y quién gana cada fila. Para mostrar.
- `versus.json`: los mismos promedios (`promedio.Clippy`, `promedio.Bals`) y cada corrida con sus clips.
- `<App>-<vuelta>\muestras.csv`: una muestra por segundo, con la fase (`llenando`, `grabando`, `guardando`).
- `<App>-<vuelta>\datos\`: el `clippy.log` o la salida de Bals (`salida.txt`), y la configuración que se usó.

Cuidado al comparar:
- **Bitrate y peso no tienen ganador.** Bals codifica con calidad variable (QVBR) y Clippy con bitrate fijo: si uno
  sale con mucho más bitrate, también se ve mejor y hace trabajar más al encoder. Mirá `clip_mbps` antes de decir que
  uno gasta menos placa.
- **Cuadros del clip** (`clip_fps`) por debajo de 60 es que se perdieron cuadros: pesa más que unos puntos de CPU.
- Lo que se mueve menos que la diferencia entre las vueltas de un mismo programa es ruido.
- Las filas "suben" (RAM, handles, hilos) son de pocos minutos: indicio, no prueba. Para fugas de Clippy, la skill
  `rendimiento` (`--test-soak`).

## 3. Mejorar Clippy con lo que salga

El ciclo para cada fila donde Clippy pierde por más que el ruido:

1. **Ubicar** qué parte de Clippy hace ese trabajo. Por fila:
   - Procesador grabando: el hilo de `session` en `src/record/recorder.rs`, `Readback` en `src/record/encoder.rs`
     (si cae al encoder por procesador), `src/record/audio.rs`.
   - Placa 3D o conversión: `src/record/convert.rs` (BGRA → NV12, pool, saltear cuadros repetidos) y
     `src/record/capture.rs`/`cursor.rs`.
   - Placa encoder: el bitrate (`clip_mbps`) y los ajustes del encoder en `src/record/encoder.rs`.
   - RAM: el buffer de paquetes (`trim_video` en `recorder.rs`) y el de audio (`AudioBuf` en `audio.rs`).
   - Memoria de video: texturas del pool y de la captura (`convert.rs`, `capture.rs`).
   - Hasta tener el clip / pico al guardar: `save_clip` y `Mix` en `recorder.rs`, `src/record/mux.rs`.
   - Hilos o handles: `Audio::open`, `game.rs`, los hilos de guardado.
2. **Ver cómo lo hace Bals.** Cloná su código en el scratchpad (nunca dentro del repo):
   `git clone --depth 1 https://github.com/sconelli24/bals <scratchpad>/bals`. Sus piezas equivalentes:
   - Bucle por cuadro: `src/engine.rs` (`step_cfr_tick`).
   - Captura: `src/capture/` (WGC y DXGI).
   - Conversión: `src/encode/video_processor.rs`.
   - Encoder: `src/encode/h264_mft.rs`.
   - Buffer: `src/buffer/mod.rs`.
   - MP4: `src/muxer/mod.rs`.
   - Audio: `src/audio/`, que lo codifica a AAC mientras graba, en vez de guardarlo en PCM.

   Bals no tiene licencia: leelo para entender la idea y escribila de nuevo en el estilo de Clippy. No copies código.
3. **Decidir si conviene.** Las reglas de `AGENTS.md` siguen valiendo:
   - Si la ventaja de Bals viene de hacer otra cosa, no es una mejora sino un cambio de la experiencia. Por ejemplo,
     grabar a menos bitrate, no grabar el escritorio o perder calidad. Planteáselo al usuario en vez de copiarlo.
4. **Cambiar** en un worktree propio (`AGENTS.md`, "Flujo de trabajo"), de a un cambio por commit.
5. **Volver a medir**:
   - `cargo build --release` en el worktree.
   - `tools/versus.ps1 -Clippy <exe del worktree> -Vueltas 2` contra el mismo Bals.
   - Para que no se cuele una fuga, también `--test-soak` (skill `rendimiento`).
   - Si la fila no mejoró más que el ruido, el cambio no va.

## 4. Qué contarle al usuario

- La tabla resumida: dónde gana cada uno y por cuánto, con los ajustes, la escena y la PC.
- Lo que explica cada diferencia grande (por ejemplo, "Bals graba a 6 Mbps reales y Clippy a 10: por eso usa menos
  encoder").
- Para cada fila donde Clippy pierde: qué cambiarías, en qué archivo, cuánto esperás ganar y si cambia algo de lo que
  ve el usuario.
- Lo que no se pudo medir (sin `ffprobe`, un programa que no arrancó, una corrida que se cayó).
