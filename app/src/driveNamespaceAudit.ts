/** Strict read-only decoder for the native Drive Namespace Integrity Audit. */
export interface NamespaceIssue {
  severity:'warning'|'critical';
  code:string;
  kind:string;
  name:string;
  explanation:string;
}
export interface NamespaceAuditReport {
  revision:number;read_only:true;raw_files:number;visible_files:number;
  folders:number;pending:number;links:number;truncated:boolean;
  issues:NamespaceIssue[];
}
const object=(x:unknown):Record<string,unknown>|null =>
 x!==null&&typeof x==='object'&&!Array.isArray(x)?x as Record<string,unknown>:null;
export function readNamespaceAudit(raw:unknown):NamespaceAuditReport {
 const r=object(raw);
 if(!r||r.read_only!==true
   ||!Number.isSafeInteger(r.revision)||(r.revision as number)<0
   ||!['raw_files','visible_files','folders','pending','links'].every(n=>
      Number.isSafeInteger(r[n])&&(r[n] as number)>=0&&(r[n] as number)<=1_000_000)
   ||typeof r.truncated!=='boolean'
   ||!Array.isArray(r.issues)||r.issues.length>200
   ||r.issues.some(item=>{
      const issue=object(item);
      return !issue || (issue.severity!=='warning'&&issue.severity!=='critical')
        ||typeof issue.code!=='string'||!/^[a-z_]{1,64}$/.test(issue.code)
        ||typeof issue.kind!=='string'||issue.kind.length>32
        ||typeof issue.name!=='string'||issue.name.length>255
        ||typeof issue.explanation!=='string'||issue.explanation.length>512;
   }))throw Error('Native Drive namespace audit returned invalid or incomplete data.');
 return r as unknown as NamespaceAuditReport;
}
export function summarizeNamespaceAudit(report:NamespaceAuditReport){
 const critical=report.issues.filter(i=>i.severity==='critical').length;
 const warnings=report.issues.length-critical;
 return {critical,warnings,limited:report.truncated,
   readyToTrust:critical===0&&warnings===0&&!report.truncated};
}
