# Separate Questionnaire history from live Provider requests

Questionnaires and ordinary Answers belong to durable Session history, but the ability to answer belongs to a live Provider request owned by the server. Closing a Client leaves that request pending; after server restart, historical Questionnaires remain unavailable for answering unless the Provider restores their requests. Treating persisted history alone as a resumable request was rejected because it would offer Answers that no Provider is waiting to receive.

Any Client viewing the Session may submit an Answer, with the first accepted submission winning and stale submissions unable to overwrite it. Secret Answer values are sent to the requesting Provider but excluded from Suru's Transcript and logs; history records only that the secret was answered.
