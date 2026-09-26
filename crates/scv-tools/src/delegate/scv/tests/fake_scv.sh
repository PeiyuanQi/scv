#!/bin/bash
echo $$ > "@PID@"
turns=0
emit() { printf '%s\n' "$1"; }
# After a "background" turn, like a server whose job-1 runs on: once the test
# creates the finish file, the job has finished and a turn the server starts
# itself reports it, asking for an approval; once the test creates the
# settle file, that turn streams a long answer and ends.
report_later() {
  until [[ -e "@FINISH@" ]]; do sleep 0.1; done
  emit '{"type":"turn.started","request_id":"background:1","session_id":"fake-session","turn_id":"r1","seq":10,"origin":{"kind":"background","jobs":["job-1"]}}'
  emit '{"type":"approval.requested","request_id":"background:1","session_id":"fake-session","turn_id":"r1","seq":11,"approval_id":"report-1","call_id":"c","name":"bash","risk":"process","cwd":"/tmp","summary":"Run the next step"}'
  until [[ -e "@SETTLE@" ]]; do sleep 0.1; done
  for i in $(seq 1000); do
    emit '{"type":"assistant.delta","request_id":"background:1","session_id":"fake-session","turn_id":"r1","seq":12,"content":"job-1 landed, and here is a long account of it, line '$i'\n"}'
  done
  emit '{"type":"turn.completed","request_id":"background:1","session_id":"fake-session","turn_id":"r1","seq":13,"steps":1,"usage":{},"origin":{"kind":"background","jobs":["job-1"]}}'
}
while IFS= read -r line; do
  id=""
  [[ $line =~ \"request_id\":\"([^\"]*)\" ]] && id="${BASH_REMATCH[1]}"
  case "$line" in
    *'"type":"initialize"'*)
      emit '{"type":"initialized","request_id":"'$id'","protocol_version":@VERSION@,"server":{"name":"fake","version":"0"}}' ;;
    *'"type":"session.start"'*)
      [[ $line =~ \"delegation_depth\":([0-9]+) ]] && echo "${BASH_REMATCH[1]}" > "@DEPTH@"
      emit '{"type":"session.started","request_id":"'$id'","session_id":"fake-session","cwd":"/","model":"m","context_max_tokens":1,"max_server_frame_bytes":1,"max_transcript_bytes":1,"max_transcript_items":1,"max_prompt_history_bytes":1,"max_prompt_history_items":1}' ;;
    *'"type":"turn.start"'*)
      echo turn >> "@TURNS@"
      turns=$((turns+1)); turn_request=$id
      emit '{"type":"turn.started","request_id":"'$id'","session_id":"fake-session","turn_id":"t'$turns'","seq":1}'
      # A "warm up" turn finishes at once in every mode, so a test can start
      # the child before timing the turn it cares about.
      if [[ $line == *'"prompt":"warm up"'* ]]; then
        emit '{"type":"assistant.completed","request_id":"'$id'","session_id":"fake-session","turn_id":"t'$turns'","seq":2,"content":"warm"}'
        emit '{"type":"turn.completed","request_id":"'$id'","session_id":"fake-session","turn_id":"t'$turns'","seq":3,"steps":1,"usage":{}}'
        continue
      fi
      case "@MODE@" in
        echo)
          emit '{"type":"assistant.delta","request_id":"'$id'","session_id":"fake-session","turn_id":"t'$turns'","seq":2,"content":"thinking\n"}'
          emit '{"type":"tool.started","request_id":"'$id'","session_id":"fake-session","turn_id":"t'$turns'","seq":3,"call_id":"c","name":"bash"}'
          emit '{"type":"tool.progress","request_id":"other","session_id":"fake-session","turn_id":"t0","seq":4,"call_id":"c","text":"stale event"}'
          emit '{"type":"tool.completed","request_id":"'$id'","session_id":"fake-session","turn_id":"t'$turns'","seq":5,"call_id":"c","name":"bash","success":true,"output":"PRIVATE","truncated":false}'
          emit '{"type":"assistant.completed","request_id":"'$id'","session_id":"fake-session","turn_id":"t'$turns'","seq":6,"content":"reply '$turns'"}'
          emit '{"type":"turn.completed","request_id":"'$id'","session_id":"fake-session","turn_id":"t'$turns'","seq":7,"steps":1,"usage":{"input_tokens":3,"output_tokens":4}}' ;;
        background)
          emit '{"type":"tool.completed","request_id":"'$id'","session_id":"fake-session","turn_id":"t'$turns'","seq":2,"call_id":"c","name":"agent","success":true,"output":"{}","truncated":false,"jobs":[{"job":"job-1","tool":"agent","agent":"codex","status":"running","task":"Land it"}]}'
          emit '{"type":"assistant.completed","request_id":"'$id'","session_id":"fake-session","turn_id":"t'$turns'","seq":3,"content":"started job-1"}'
          emit '{"type":"turn.completed","request_id":"'$id'","session_id":"fake-session","turn_id":"t'$turns'","seq":4,"steps":1,"usage":{}}'
          report_later & ;;
        approve)
          emit '{"type":"approval.requested","request_id":"'$id'","session_id":"fake-session","turn_id":"t'$turns'","seq":2,"approval_id":"a1","call_id":"c","name":"bash","risk":"process","cwd":"/tmp","summary":"Run rm -rf build"}' ;;
        die)
          echo "boom: provider unreachable" >&2
          exit 3 ;;
      esac ;;
    *'"type":"approval.resolve"'*)
      if [[ $line == *'"approved":true'* ]]; then answer=approved; else answer=denied; fi
      if [[ $line == *'"approval_id":"report-1"'* ]]; then
        echo $answer >> "@ANSWERS@"
        continue
      fi
      emit '{"type":"assistant.completed","request_id":"'$turn_request'","session_id":"fake-session","turn_id":"t'$turns'","seq":3,"content":"'$answer'"}'
      emit '{"type":"turn.completed","request_id":"'$turn_request'","session_id":"fake-session","turn_id":"t'$turns'","seq":4,"steps":1,"usage":{}}' ;;
    *'"type":"turn.cancel"'*)
      echo cancel >> "@CANCELS@"
      if [[ "@MODE@" == cancellable ]]; then
        emit '{"type":"turn.cancelled","request_id":"'$turn_request'","session_id":"fake-session","turn_id":"t'$turns'","seq":9}'
      fi ;;
  esac
done
