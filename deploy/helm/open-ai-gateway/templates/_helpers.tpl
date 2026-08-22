{{- define "oag.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "oag.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name (include "oag.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "oag.labels" -}}
app.kubernetes.io/name: {{ include "oag.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end -}}

{{- define "oag.selectorLabels" -}}
app.kubernetes.io/name: {{ include "oag.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "oag.image" -}}
{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}
{{- end -}}

{{/*
Preflight checks.

These are the mistakes that do not fail loudly at runtime — they fail as severed
streams, or as a gateway that boots and then rejects every request. Refusing to
render is the cheapest possible place to catch them.
*/}}
{{- define "oag.validate" -}}
  {{- $needed := add (int .Values.gateway.maxStreamDurationSeconds) (int .Values.preStopDelaySeconds) -}}
  {{- if lt (int .Values.terminationGracePeriodSeconds) $needed -}}
    {{- fail (printf "terminationGracePeriodSeconds (%v) is less than preStopDelaySeconds + gateway.maxStreamDurationSeconds (%v + %v = %v). Kubernetes would kill the pod while it is still draining, severing live streams on every rolling update — and to each client that looks like a random upstream failure rather than a deploy."
        .Values.terminationGracePeriodSeconds .Values.preStopDelaySeconds .Values.gateway.maxStreamDurationSeconds $needed) -}}
  {{- end -}}

  {{- if not (or .Values.security.existingSecret (and .Values.security.signingSecret .Values.security.credentialKek)) -}}
    {{- fail "security.signingSecret and security.credentialKek are required (or set security.existingSecret). They must be byte-identical across replicas: with different values, tokens minted by one pod are rejected by the next. Generate with `openssl rand -base64 48` and `openssl rand -base64 32`." -}}
  {{- end -}}

  {{- if eq .Values.data.mode "external" -}}
    {{- if not (or .Values.data.external.existingSecret (and .Values.data.external.databaseUrl .Values.data.external.redisUrl)) -}}
      {{- fail "data.mode=external needs data.external.databaseUrl and data.external.redisUrl, or data.external.existingSecret." -}}
    {{- end -}}
  {{- else if ne .Values.data.mode "inCluster" -}}
    {{- fail (printf "data.mode must be 'external' or 'inCluster', not %q" .Values.data.mode) -}}
  {{- end -}}

  {{- if and .Values.podDisruptionBudget.enabled (ge (int .Values.podDisruptionBudget.minAvailable) (int .Values.replicaCount)) -}}
    {{- if not .Values.autoscaling.enabled -}}
      {{- fail (printf "podDisruptionBudget.minAvailable (%v) is not below replicaCount (%v); no pod could ever be evicted, so node drains would hang."
          .Values.podDisruptionBudget.minAvailable .Values.replicaCount) -}}
    {{- end -}}
  {{- end -}}
{{- end -}}

{{/* The name of the secret holding the database and redis URLs. */}}
{{- define "oag.dataSecret" -}}
{{- if eq .Values.data.mode "inCluster" -}}
{{ include "oag.fullname" . }}-data
{{- else if .Values.data.external.existingSecret -}}
{{ .Values.data.external.existingSecret }}
{{- else -}}
{{ include "oag.fullname" . }}-data
{{- end -}}
{{- end -}}

{{- define "oag.securitySecret" -}}
{{- if .Values.security.existingSecret -}}
{{ .Values.security.existingSecret }}
{{- else -}}
{{ include "oag.fullname" . }}-security
{{- end -}}
{{- end -}}

{{/* Environment shared by the gateway and the migration job. */}}
{{- define "oag.env" -}}
- name: OAG_DATABASE__URL
  valueFrom:
    secretKeyRef:
      name: {{ include "oag.dataSecret" . }}
      key: OAG_DATABASE__URL
- name: OAG_REDIS__URL
  valueFrom:
    secretKeyRef:
      name: {{ include "oag.dataSecret" . }}
      key: OAG_REDIS__URL
- name: OAG_SECURITY__SIGNING_SECRET
  valueFrom:
    secretKeyRef:
      name: {{ include "oag.securitySecret" . }}
      key: OAG_SECURITY__SIGNING_SECRET
- name: OAG_SECURITY__CREDENTIAL_KEK
  valueFrom:
    secretKeyRef:
      name: {{ include "oag.securitySecret" . }}
      key: OAG_SECURITY__CREDENTIAL_KEK
{{- end -}}
