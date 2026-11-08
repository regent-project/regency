{{- define "regency.fullname" -}}
{{- if contains "regency" .Release.Name -}}
{{- .Release.Name | trunc 50 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-regency" .Release.Name | trunc 50 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "regency.labels" -}}
app.kubernetes.io/part-of: regency
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
{{- end -}}

{{- define "regency.secretName" -}}
{{- if .Values.existingSecret -}}{{ .Values.existingSecret }}{{- else -}}{{ include "regency.fullname" . }}-secrets{{- end -}}
{{- end -}}

{{- define "regency.rabbitmqUrl" -}}
{{- if .Values.rabbitmq.enabled -}}amqp://{{ include "regency.fullname" . }}-rabbitmq:5672{{- else -}}{{ .Values.rabbitmq.external.url }}{{- end -}}
{{- end -}}
{{- define "regency.streamHost" -}}
{{- if .Values.rabbitmq.enabled -}}{{ include "regency.fullname" . }}-rabbitmq{{- else -}}{{ .Values.rabbitmq.external.streamHost }}{{- end -}}
{{- end -}}
{{- define "regency.streamPort" -}}
{{- if .Values.rabbitmq.enabled -}}5552{{- else -}}{{ .Values.rabbitmq.external.streamPort }}{{- end -}}
{{- end -}}
{{- define "regency.managementUrl" -}}
{{- if .Values.rabbitmq.enabled -}}http://{{ include "regency.fullname" . }}-rabbitmq:15672{{- else -}}{{ .Values.rabbitmq.external.managementUrl }}{{- end -}}
{{- end -}}
{{- define "regency.couchdbUrl" -}}
{{- if .Values.couchdb.enabled -}}http://{{ include "regency.fullname" . }}-couchdb:5984{{- else -}}{{ .Values.couchdb.external.url }}{{- end -}}
{{- end -}}

{{/* a secret value kept across upgrades: existing, else random */}}
{{- define "regency.keep" -}}
{{- $existing := index . 0 -}}
{{- $key := index . 1 -}}
{{- if and $existing (hasKey $existing.data $key) -}}
{{- index $existing.data $key -}}
{{- else -}}
{{- randAlphaNum 40 | b64enc -}}
{{- end -}}
{{- end -}}
