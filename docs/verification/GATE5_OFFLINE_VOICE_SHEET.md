# Gate 5 — Verificación Presencial de Voz Offline (Solo Propuesta Firmada)

**Estado inicial**: `PENDIENTE` (requiere operador presencial, micrófono y motor ASR offline)

> [!IMPORTANT]
> Regla arquitectónica inviolable: la voz **NUNCA** ejecuta acciones directas en salidas ni autoriza cambios en RAM/relés. Toda orden de voz debe compilarse en una **propuesta candidata** (`Proposal` / `RuleSetV1` firmado o JSON canonical), pasar validación estática/esquema, preview de diff y Shadow antes de requerir autorización explícita humana (`Bearer Operator/Engineer`).

## Alcance
Validar el flujo extremo a extremo offline de voz -> intención estructurada -> propuesta `Verified` -> `Shadow` -> confirmación manual explícita en consola iPad o API.

## Precondiciones
- Pipeline ASR offline funcionando sin conexión a Internet (`airplane mode` en dispositivo operador o red aislada).
- Identidad de autor diferenciada para origen voz (`actor = "voice-assistant"`, rol `Author`).

## Procedimiento de Prueba Presencial
1. **Aislamiento de Privilegios del Token de Voz**
   - Confirmar que el cliente de voz solo posee credencial de rol `Author` (`plc-author-token`).
   - Intentar invocar desde el módulo de voz `/authorizations`, `/activate`, `/rollback`, `/rearm`, `/manual`: todos deben fallar con `403 FORBIDDEN`.
2. **Generación de Propuesta Estructurada (`Voice -> RuleSet IR`)**
   - Dictar comando de umbral/temporizador en ambiente controlado (ej. *"Ajustar umbral rojo a 44 grados tras 8 segundos"*).
   - Verificar creación de transacción en estado `Verified` mediante `POST /v1/devices/{id}/proposals` con `baseRevision` exacto y firma criptográfica Ed25519 válida.
3. **Preview & Verificación Shadow Obligatoria**
   - Ejecutar `POST /v1/proposals/{txId}/shadow` y revisar visualmente diff y resultado antes de autorizar.
4. **Rechazo Seguro ante Ambigüedad o Ruido**
   - Introducir audio ininteligible, ruido industrial o comando fuera de rango físico (ej. umbral negativo extremo o capacidad prohibida).
   - Confirmar que el parser/supervisor rechaza antes de `Armed` (`400`/`422`) y que la lógica activa en ejecución no sufre ninguna perturbación (`0` cambios en `activeRevision` ni `appliedMask`).
5. **Confirmación Presencial Explícita**
   - Operador con `plc-operator-token` autoriza (`packageHash` pinneado + `ttlMs`) y activa en frontera de scan, o cancela la propuesta verificando transición limpia a `Rejected`.

## Criterios de Aceptación
- El subsistema de voz opera `100%` offline y con rol máximo `Author`.
- Toda orden ambigua/corrupta es rechazada sin impactar el lazo RT activo.
- Ningún comando de voz puede energizar o conmutar una salida sin autorización explícita de operador humano.
- Registro firmado por operador presencial: Fecha `____-__-__`, Operador `__________`, Resultado `PENDIENTE / PASS / FAIL`.
