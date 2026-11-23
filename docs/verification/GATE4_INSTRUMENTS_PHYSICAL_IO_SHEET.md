# Gate 4 — Verificación Presencial con Instrumentos y E/S Física

**Estado inicial**: `PENDIENTE` (requiere operador presencial, fuente, multímetro/osciloscopio, relé físico y sensor THT02 RS485)

> [!CAUTION]
> En esta prueba SÍ se habilita E/S física (`--hardware-gpio` y/o `--modbus-dev`). Verificar primero que `lyra-plc.service` (firmware stock de planta) esté coordinado y que no haya cargas peligrosas energizadas sin paro de emergencia físico cableado.

## Alcance
Validar en hardware real de laboratorio: GPIO chardev v2 (`GPIO 17`/`18` + línea de inhibición física), bus RS485 Modbus RTU THT02, precedencia del `ReactiveShield`, Dwell/anti-chatter con osciloscopio y corte físico ante fallo de trabajador o inhibición.

## Instrumentación Requerida
- Osciloscopio digital de 2+ canales conectado a salida digital (`DO` relé) e entrada de inhibición física (`SIGNAL_PHYSICAL_INHIBIT`).
- Sensor Modbus RS485 THT02 conectado al adaptador UART/RS485 (`--modbus-dev /dev/ttyS...`).
- Cámara térmica o termómetro de referencia para validar lectura `THT02` vs telemetría interna.

## Procedimiento de Prueba Presencial
1. **Arranque Seguro Desarmado (`StartupDisarmed`)**
   - Iniciar `plc-supervisor` SIN `--auto-arm`, CON `--hardware-gpio --modbus-dev /dev/ttyS...`.
   - Confirmar en osciloscopio que las salidas físicas permanecen en estado seguro (`0 V` / desenergizado) hasta `/rearm` explícito.
2. **Frescura RS485 & Señal Stale**
   - Desconectar físicamente cable `A/B` del sensor THT02; verificar transición `Good -> Stale` tras TTL de perfil (`> sample_age_ms`) y caída inmediata de la salida a estado seguro (`GuardReason::InputQualityNotGood`). Reconectar y validar recuperación controlada.
3. **Inhibición Física & Precedencia sobre Dwell (`F05`)**
   - Energizar salida mediante regla/manual lease; dentro de la ventana dwell (`< 500 ms`), intentar conmutación normal por lógica/API y medir en osciloscopio que el relé sostiene estado sin chattering (`GuardReason::DwellChatter`).
   - Activar entrada física de inhibición dentro de la ventana dwell y medir tiempo de corte hardware (`target <= 1 ciclo de scan + driver`). Confirmar que contingencia OFF **siempre** anula dwell.
4. **Descrepancia Testigo (`D04` / Relé soldado)**
   - Simular/forzar discrepancia entre máscara comandada y lectura de contacto auxiliar y confirmar latch inmediato a fallback seguro.
5. **Estabilidad Eléctrica & Latencia Real con E/S Física**
   - Medir con osciloscopio + métricas internas `/v1/devices/{id}/night-metrics` que los tiempos de ciclo con E/S física cumplen `eval_deadline_ms` sin `missed_releases`.

## Criterios de Aceptación
- Arranque siempre desarmado con salidas físicas en `0`.
- Precedencia completa `PhysicalInhibit > FallbackLatched > Quality > Worker > Interlock > Dwell` observada en osciloscopio.
- Tiempo de corte físico ante inhibición medido y documentado: `______ ms`.
- Registro firmado por operador presencial: Fecha `____-__-__`, Instrumentos `__________`, Resultado `PENDIENTE / PASS / FAIL`.
