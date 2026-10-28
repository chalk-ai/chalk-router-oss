{{- define "chalk-router.commonLabels" -}}
app.kubernetes.io/name: chalk-router
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version | replace "+" "_" }}
{{- end }}

{{- /* Shared by the Deployment and the migrate Job: the Job needs the mode and
environment for its configuration check, never metadata API access. */ -}}
{{- define "chalk-router.environmentEnv" -}}
- name: ROUTER_TENANCY_MODE
  value: {{ .Values.backend.tenancyMode | quote }}
{{- if .Values.backend.legacyEnvironmentId }}
- name: CHALK_ENVIRONMENT_ID
  value: {{ .Values.backend.legacyEnvironmentId | quote }}
{{- end }}
{{- end }}

{{- define "chalk-router.tenancyEnv" -}}
{{ include "chalk-router.environmentEnv" . }}
{{- if eq .Values.backend.tenancyMode "cluster" }}
- name: ROUTER_METADATA_API_ENDPOINT
  value: {{ required "backend.metadataApiEndpoint is required in cluster mode" .Values.backend.metadataApiEndpoint | quote }}
{{- if .Values.backend.kubeClusterId }}
- name: ROUTER_KUBE_CLUSTER_ID
  value: {{ .Values.backend.kubeClusterId | quote }}
{{- end }}
{{- end }}
{{- end }}

{{- define "chalk-router.backendEnv" -}}
{{ include "chalk-router.tenancyEnv" . }}
- name: ROUTER_ACCEPT_CROSS_ENVIRONMENT_TOKENS
  value: {{ .Values.backend.auth.acceptCrossEnvironmentTokens | quote }}
- name: ROUTER_USAGE_BACKEND
  value: "postgres"
- name: ROUTER_QUOTA_BACKEND
  value: "postgres"
- name: CHALK_JWT_ISSUERS
  value: {{ .Values.backend.auth.jwtIssuers | quote }}
- name: CHALK_JWT_AUDIENCE
  value: {{ if .Values.backend.auth.jwtIssuers }}{{ required "backend.auth.jwtAudience is required with jwtIssuers" .Values.backend.auth.jwtAudience | quote }}{{ else }}""{{ end }}
- name: CHALK_TEAM_ID
  value: {{ if and .Values.backend.auth.jwtIssuers (eq .Values.backend.tenancyMode "environment") }}{{ required "backend.auth.teamId is required for environment-mode JWT administration" .Values.backend.auth.teamId | quote }}{{ else }}{{ .Values.backend.auth.teamId | quote }}{{ end }}
- name: ROUTER_TRACE_LOG_CONTENT
  value: {{ .Values.backend.telemetry.logContent | quote }}
{{- with .Values.backend.telemetry.otlpEndpoint }}
- name: OTEL_EXPORTER_OTLP_ENDPOINT
  value: {{ . | quote }}
{{- end }}
{{- end }}

{{/* Runtime identity and containment shared by all pods and containers. */}}
{{- define "chalk-router.podSecurity" -}}
runAsNonRoot: true
runAsUser: 10001
runAsGroup: 10001
fsGroup: 10001
fsGroupChangePolicy: OnRootMismatch
seccompProfile:
  type: RuntimeDefault
{{- end -}}

{{- define "chalk-router.containerSecurity" -}}
allowPrivilegeEscalation: false
readOnlyRootFilesystem: true
capabilities:
  drop: [ALL]
{{- end -}}
