# Gate 3 — Verificación Presencial de Acceso iPad & Consola Offline Zero-CDN

**Estado inicial**: `PENDIENTE` (requiere operador presencial con iPad en red local/tailnet)

## Alcance
Validar acceso real desde iPad (Safari / iPadOS) hacia la consola offline servida por `plc-supervisor` sin recursos externos (zero-CDN), con separación estricta de roles Bearer RBAC y visibilidad en vivo de telemetría/evidencia/auditoría.

## Precondiciones
- Equipo Luckfox Lyra corriendo perfil diurno/presencial en IP conocida de LAN o Tailscale (ej. `<LYRA_LAN_IP>:8443` detrás de reverse proxy TLS local o túnel seguro).
- Bundle offline compilado en `apps/console/dist-web` pasado con `--console-dir`.
- Tokens RBAC diferenciados cargados para `Reader`, `Author`, `Operator` y `Engineer` (sin compartir token administrador al navegador de lectura).

## Procedimiento de Prueba Presencial
1. **Zero-CDN & Carga Offline**
   - Abrir Safari Web Inspector / proxy local en iPad y cargar `/console/`.
   - Confirmar que todas las peticiones son origin-local (`0` peticiones a Internet/CDN) y el código HTTP es `200`.
2. **Validación RBAC (`Reader` vs `Author` vs `Operator`)**
   - Con `plc-reader-token`: verificar lectura de estado, scans recientes, evidencia e historial de auditoría (`200`). Intentar `/proposals`, `/authorize`, `/activate`, `/rearm`, `/manual` y confirmar rechazo `403 FORBIDDEN`.
   - Sin cabecera `Authorization`: confirmar `401 UNAUTHORIZED`.
   - Con `plc-author-token`: proponer un cambio firmado válido (`201`) e intentar autorizar/activar (`403 FORBIDDEN`).
3. **Flujo Completo en Consola desde iPad**
   - Verificar que el banner de modo (`virtual` o `hardware`), los 4 badges de evidencia (`Selected-In-Scan`, `Active-Persisted`, `Driver-Applied`, `Physical-Observed`), el botón de emergencia safe-OFF y el visor de auditoría reflejan los cambios en `< 2 s`.
4. **Resiliencia ante Corte de Red Wi-Fi del iPad (D02)**
   - Activar modo avión en el iPad justo después de enviar propuesta con `txId` fijo; reconectar Wi-Fi y reintentar la misma propuesta (`txId` idempotente) verificando que no duplica revisión ni corrompe estado.

## Criterios de Aceptación
- `0` peticiones externas / CDN.
- `401`/`403` estrictos según rol.
- Idempotencia `D02` comprobada sin doble promoción.
- Registro firmado por operador presencial: Fecha `____-__-__`, Operador `__________`, Resultado `PENDIENTE / PASS / FAIL`.
