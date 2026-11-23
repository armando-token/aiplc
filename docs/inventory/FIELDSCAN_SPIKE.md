# Spike de Integración — FieldScan → PLC Automático (`apps/ipad`)

**Fuente revisada:** `/Users/armandosilva/Projects/Fiedscan/MEGAPLAN_FieldScan_Implementacion_y_Validacion.md`  
**Fecha:** 2026-11-22

---

## 1. Hallazgo de inventario
El directorio `/Users/armandosilva/Projects/Fiedscan` contiene actualmente el documento de especificación `MEGAPLAN_FieldScan_Implementacion_y_Validacion.md` (no contiene aún un árbol `.xcodeproj` ni fuentes `.swift` locales en esa carpeta; el megaplan de FieldScan referencia `Projects/Beta_2` y la app instalada en el iPad Air M4).

Por lo tanto, `apps/ipad` en `PLC_Lucky` implementa un **Swift Package autónomo y reproducible** (`PlcAutoCore` + `PlcAutoUI` + `PlcAutoCoreTests` ejecutables vía `swift test` en macOS/iPadOS) adoptando directamente los patrones arquitectónicos y de seguridad operativa definidos en FieldScan.

---

## 2. Piezas y patrones reutilizados desde FieldScan

| Área en FieldScan | Cómo se adapta y reutiliza en `PLC_Lucky/apps/ipad` |
| --- | --- |
| **Voz local offline (`VOZ-01`..`VOZ-10`)** | Uso de `SFSpeechRecognizer` en `es-PE` verificando `supportsOnDeviceRecognition` y exigiendo `requiresOnDeviceRecognition = true`. Modo pulsar-para-hablar (push-to-talk) con bloqueo de captura mientras el altavoz/TTS esté activo para evitar auto-confirmaciones por eco, y fallback inmediato a entrada textual si falta permiso o modelo local. |
| **Normalización y desambiguación estricta** | El normalizador de texto/voz produce intenciones tipadas deterministas. Si hay ambigüedad de unidad, canal, temporizador o alcance (ej. *"Cámbiame la temperatura"*), no se envía ni autoriza ninguna transacción hasta recibir aclaración del operador. |
| **Idempotencia y recuperación de `txId` (`DAT-08`)** | Persistencia local en disco de transacciones (`txId`, `baseRevision`, `proposalHash`, estado). Ante timeout o caída de Wi-Fi/tether, la app entra en estado `"verificando estado"` y consulta `GET /v1/proposals/{tx}` con la misma clave sin disparar una segunda activación. |
| **Separación de evidencia y frescura (`DAT-02`, `DAT-03`)** | Las lecturas de temperatura distinguen `GOOD`, `STALE`, `INVALID` y `UNAVAILABLE`, preservando marcas de tiempo originales y nunca sustituyendo silencio/fallo por `0 °C`. Se muestran por separado las 4 dimensiones de evidencia del PLC: `SELECTED_IN_SCAN`, `DRIVER_APPLIED`, `PHYSICAL_OBSERVED` y `ACTIVE_PERSISTED`. |
| **Operación sin WAN (`OFF-01`..`OFF-08`)** | Catálogo local versionado embebido en el cliente, sin recursos ni CDN externos, y soporte para TLS local con dominio de prueba o túnel USB. |

---

## 3. Aislamiento estricto: Telemetría de sensores vs API de propuestas

Siguiendo la sección §6.4.5 del megaplan de `PLC_Lucky` y la puerta de seguridad funcional de FieldScan (`BUS-12` / `VOZ-07`):

1. **Rutas de solo lectura aisladas (`TelemetryClient`):**
   - `GET /v1/devices/{id}/state`
   - `GET /v1/devices/{id}/telemetry`
   - Estas rutas alimentan únicamente los indicadores de temperatura, calidad, estado de salida física/virtual (`tower.blue` marcada explícitamente como `VIRTUAL`) y latido. Nunca mutan reglas ni estados de control.
2. **Rutas de cambio gobernadas por doble confirmación (`ProposalClient`):**
   - `POST /v1/devices/{id}/proposals` (requiere `baseRevision` exacta y produce diff + simulación).
   - `POST /v1/proposals/{tx}/authorizations` (requiere rol explícito y hash + TTL).
   - `POST /v1/proposals/{tx}/activate` (solo tras confirmación humana explícita en la pantalla de diff; la captura de voz por sí sola jamás llama `authorize` ni `activate`).
   - `POST /v1/proposals/{tx}/cancel` y `POST /v1/devices/{id}/rollbacks`.
3. **Separación de permisos y credenciales:** El cliente separa `reader` (lectura continua de telemetría) de `author` / `operator` / `engineer` (tokens/roles distintos por operación).
