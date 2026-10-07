%token NUM ID
%left '+' '-'
%left '*'
%nonassoc '<'
%right UMINUS
%%
s: e | s ';' e ;
e: e '+' e | e '-' e | e '*' e | e '<' e | '-' e %prec UMINUS | NUM | ID { x = 1; } '(' args ')' | ;
args: /* empty */ | e | args ',' e ;
%%
