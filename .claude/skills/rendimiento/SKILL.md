---
name: rendimiento
description: Medir y evaluar el rendimiento de la grabadora de Clippy en Windows (procesador, RAM, memoria de video, uso de la placa, cuadros perdidos, tiempos al guardar) y buscar fugas de memoria, handles o hilos en una grabación larga. Usala cuando pidan medir, comparar o vigilar el rendimiento, buscar memory leaks, ver si un cambio mejoró o empeoró algo, o decidir dónde optimizar.
---

# Rendimiento de la grabadora

La grabadora se mide a sí misma con `--test-soak`: graba como en el uso real, guarda un clip cada tanto y cada segundo
anota lo que usa. Al final busca fugas y deja un veredicto. Todo es solo de Windows y del build con `devtools`.

## 1. Preparar

- Compilá en el `target/` de este worktree: `cargo build --profile fast --features devtools`.
- Cada corrida va en una carpeta nueva, con su propio `APPDATA`, así el `config.json` es el de fábrica y el
  `clippy.log` es solo de esa corrida. Por ejemplo `C:\Users\<usuario>\AppData\Local\Temp\clippy-perf\<fecha>-<escenario>`.
- **De a una por vez** y sin otras mediciones al mismo tiempo: los números de procesador y placa se mezclan. Si el
  Clippy del usuario está grabando, avisale que los números van a salir mezclados con los suyos o pedile que lo cierre.

## 2. Correr

En bash (Git Bash). El exe no tiene consola: la salida se ve redirigiéndola.

```bash
D="$TEMP/clippy-perf/$(date +%Y%m%d-%H%M%S)-escritorio"; mkdir -p "$D/datos"
APPDATA="$D/datos" target/fast/clippy.exe --test-soak 10 "$D" desktop 60 > "$D/consola.txt" 2>&1
```

Los argumentos son `<minutos> <carpeta> [escenario|-] [segundos entre clips]`. Con `-` queda el escenario de fábrica
y podés pasar igual los segundos.

- **Duración**: para confirmar fugas de RAM ("RAM fuera de los buffers"), al menos 30 minutos (en corridas más cortas
  el heap sube y baja ~40 MB sin fuga y da "corrida corta para saber"). Para handles, hilos y VRAM alcanza con 10 minutos
  (el buffer de fábrica tarda 30 s más 15 s en llenarse). Para una comparación rápida de consumo alcanza con 3 minutos.
- **Escenarios** (los mismos de `--test`):
  - `desktop`: el escritorio y el audio del sistema.
  - `game`: el canal del juego.
  - `mic`: el micrófono.
  - `juegos`: el modo "Solo juegos".
  - `cursor`: el cursor dibujado.
  - `hevc`: H.265.
  - `720`, `1080` o `1440`: la resolución del clip.
- **Encoder por procesador**: agregá `CLIPPY_CPU_ENCODER=1`.
- **Con un juego**: corré a la par el juego de mentira desde una copia del exe con otro nombre (si se llama "clippy",
  no cuenta como juego). Cambia el modo de video del monitor, así que avisale al usuario antes.

  ```bash
  cp target/fast/clippy.exe "$D/juego.exe"
  "$D/juego.exe" --test-fullscreen 1920x1080 660 &
  ```

  Que dure un poco más que la medición: 660 s para 10 min.

### Vigilar mientras graba

Lanzá la corrida en segundo plano y seguí `consola.txt`. Cada minuto aparece una línea `ESTADO` con procesador, RAM,
memoria de video, placa, handles e hilos. Una línea `AVISO` aparece en cuanto algo se sale de lo normal: el encoder
recibe menos cuadros, se saltean cuadros, una vuelta del hilo tarda más que un cuadro o la RAM salta sin guardar un clip.

Con la herramienta Monitor: `tail -f "$D/consola.txt" | grep --line-buffered -E "ESTADO|AVISO|ERROR|Rendimiento"`.
No pidas más que eso: cada segundo ya queda guardado en `perf.jsonl`.

## 3. Leer el resultado

En la carpeta quedan:
- `informe.md`: para leer y mostrar.
- `informe.json`: los mismos datos, para comparar.
- `perf.jsonl`: una muestra por segundo.

El exe sale con 0 si todo está bien, con 3 si hay algo para revisar y con 1 si no pudo medir.

- `veredicto` y `avisos` primero.
- **Fugas**: el piso de cada tramo de 30 s (sin los picos de guardar), desde que el buffer está lleno.
  - "posible fuga": sube más de lo tolerado por minuto y en total.
  - "RAM fuera de los buffers de video y audio" es la clave. Si sube la RAM privada pero esta no, es el buffer creciendo
    (por ejemplo, un video que se mueve más y pesa más), no una fuga.
  - En "RAM fuera de los buffers" no se miran los tramos con el buffer achicado (por debajo del 75 % del más alto hasta
    ahí, por ejemplo con el juego en pausa), se usa la envolvente inferior (mínimo de cada tramo y todos los siguientes,
    así los saltos temporales del heap que vuelven a bajar no suman) y solo se confirma fuga en corridas de 30 min o más.
    `tramos_descartados` dice cuántos tramos con buffer achicado se omitieron.
  - `al_cerrar`: hilos y handles que quedaron después de cerrar la grabadora. Antes de medir, el soak hace una vuelta de
    calentamiento de 5 s con un clip (en `calentamiento\`, no cuenta entre los clips de la corrida), así que lo que
    Windows carga una sola vez (las DLL de Media Foundation y del encoder, ~95 handles) ya no aparece.
- **Tirones** (`tirones`): los segundos con menos del 90 % de los cuadros al encoder, cuándo fueron y la causa que más
  se repitió en esos segundos. Lo cuenta el hilo de grabación en cada vuelta (`gpu_late`, `encoder_busy`, `dropped` en
  `perf.jsonl`):
  - `conversion_sin_terminar`: el cuadro más viejo de la fila seguía sin convertir. La placa atiende la conversión
    detrás del juego.
  - `encoder_sin_pedir`: había cuadros listos y el encoder no pidió más (encoder saturado).
  - `descartados_fila_llena`: la fila de conversiones (`IN_FLIGHT`) estaba llena y el cuadro nuevo se perdió.
  - Si no hay ninguna de las tres, no llegaron imágenes nuevas (la captura no entregó).
- `cpu_muestreo_pct` es lo que costó medir (sobre todo los contadores de la placa). Ya está descontado de `cpu_pct`.
- Las dudas se responden en `perf.jsonl`: picos alrededor de cada `guardando: true`, saltos, cuándo empezó a crecer algo.

## 4. Dónde mirar según lo que salga

| Síntoma | Dónde mirar |
|---|---|
| Sube la RAM fuera de los buffers | Paquetes que no se sueltan (`trim_video` en `record/recorder.rs`), muestras de Media Foundation sin liberar (`record/encoder.rs`), hilos de guardado (`record/mux.rs`), bloques de audio (`record/audio.rs`). |
| Sube la memoria de video | Texturas que se recrean: el pool de `Converter` (`record/convert.rs`), la copia de la captura y el cursor (`record/capture.rs`, `record/cursor.rs`). |
| Suben los hilos o los handles | Hilos de audio (`Audio::open`, `record/audio.rs`), eventos de WASAPI, procesos abiertos en `record/game.rs`, hilos de guardado. |
| Tirones por conversión sin terminar | La placa atiende la conversión detrás del juego: prioridad de Clippy en la placa (`Capture::open_on` en `record/capture.rs`). |
| Tirones por encoder sin pedir o fila llena | El encoder no da abasto un rato o comparte el dispositivo D3D11 con la placa ocupada por el juego: revisar `IN_FLIGHT` en `record/convert.rs`, `pool` en `session` (`record/recorder.rs`) y la prioridad en la placa (`Capture::open_on` en `record/capture.rs`). |
| `fps_real` por debajo de lo pedido | Encoder saturado (`pending_input`), conversiones en vuelo llenas (`IN_FLIGHT` en `record/convert.rs`), placa al 100 %. |
| Trabajo por cuadro alto o cuadros salteados | Lo que corre en cada vuelta de `session` (`record/recorder.rs`): `au.sync`, `game.poll`, `cap.aim` cada 250 ms, `save_clip`. |
| Grabación frenada al guardar | `save_clip` en el hilo de grabación (`record/recorder.rs`). |
| Mucho procesador con encoder por procesador | `Readback` y el encoder de Windows (`record/encoder.rs`). |

### Handles o hilos que quedan al cerrar

Si `al_cerrar` muestra handles o hilos de más, corré `--test-restart [vueltas] <carpeta>` (con su propio `APPDATA`).
Arranca la grabadora, graba 15 s, guarda un clip y la cierra, 3 veces si no se dice cuántas, en el mismo proceso.
Después de cada vuelta cuenta lo que quedó:
- **Igual en cada vuelta**: es de una sola vez (las DLL de Media Foundation o del encoder de la placa) y no es una fuga.
- **Crece en cada vuelta**: cada sesión deja algo sin soltar. Sale con 3.

## 5. Comparar contra otra versión

Para saber si un cambio mejoró algo, medí las dos versiones en las mismas condiciones: mismo escenario, mismos minutos,
misma PC y nada más abierto.

1. La versión anterior va en su propio worktree: `git worktree add .claude/worktrees/perf-base <commit>`. Ahí,
   `cargo build --profile fast --features devtools`. Antes del commit que trajo `--test-soak` ese modo no existe: para
   esas versiones usá `tools/medir.ps1`.
2. Corré el mismo comando con cada exe, uno después del otro.
3. Compará los `informe.json` campo por campo y armá una tabla con antes, después y diferencia en %, sobre todo
   `cpu_pct.prom`, `gpu_pct.prom`, `ram_privada_mb.prom`, `vram_mb.prom`, `fps_real.prom`,
   `trabajo_por_cuadro_ms.p95` y `guardado_frenada_ms.max`.
4. Una diferencia menor que la que hay entre dos corridas de la misma versión es ruido. Si importa, repetí cada
   corrida dos veces.

## 6. Qué contarle al usuario

- El veredicto y los avisos, en palabras: qué pasa y qué se nota al usarlo.
- Los números clave, con su escenario.
- Si hay una fuga: cuánto crece por minuto y cuánto sería en una hora de juego.
- Dónde mejorar: lo que más consume o lo que se acerca al límite, con el archivo y la función, y qué cambiarías.
- Lo que no se pudo medir y por qué.

## 7. Medir cuánto le baja Clippy los FPS al juego

`--test-soak` mide a Clippy desde adentro, pero no ve los FPS del juego. Para medir el impacto real sobre el juego (por
ejemplo, para validar si subir la prioridad de Clippy en la placa le saca rendimiento al juego):

1. Usá una herramienta externa de medición, nunca como dependencia de Clippy:
   - **PresentMon** (Intel, CLI o GUI): mide FPS promedio, 1 % low y 0,1 % low (`msBetweenPresents` / `msBetweenDisplayChange`)
     del proceso del juego sin inyectarse en él (seguro con anticheats como Vanguard).
   - O el contador/benchmark integrado del juego si reporta promedio y percentiles.
2. Misma partida, mismo mapa o recorrido repetible, mismos ajustes gráficos y nada más abierto:
   - **Línea base**: sin Clippy abierto.
   - **Con Clippy**: grabando en segundo plano con su `APPDATA` de prueba (y, si se compara una opción como prioridad
     `realtime` vs `high`, una corrida por variante).
3. Compará FPS promedio, 1 % low y 0,1 % low (el 1 % low muestra micro-tirones que el promedio tapa). Si la diferencia es
   menor al ruido entre dos pasadas de la misma condición, repetí cada condición dos veces.

## 8. Casos pendientes de medir

Además de NVIDIA + H.264 jugando como administrador, falta cubrir estos escenarios en Windows:

- **Valorant (Vanguard + resolución estirada 1568×1080)**: anticheat estricto (verifica que el detector de juegos y el
  audio del juego no fallen sin abrir el proceso) y reescalado/estirado de pantalla completa exclusiva + Alt+Tab.
- **Placas AMD (AMF) e Intel (Quick Sync)**: tiempos de conversión, latencia del encoder, tirones, VRAM y encabezados
  H.264/H.265 en drivers reales.
- **Encoder por procesador (`CLIPPY_CPU_ENCODER=1`)**: consumo de CPU grabando y jugando, `Readback` y cuadros perdidos
  a 1080p 30 fps.
- **Clippy como usuario común (sin permisos de administrador)**: comportamiento habitual del usuario final, permisos de
  prioridad en la placa (`D3DKMTSetProcessSchedulingPriorityClass` / `SetGPUThreadPriority`) y captura sobre juegos
  elevados.
